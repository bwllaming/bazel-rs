//! Native extraction into caller-owned, private staging directories.
//!
//! Symbolic links (tar type `2`, zip `S_IFLNK` entries) and tar hard links
//! (type `1`) follow Bazel 9.3's decompressors (docs/ARCHIVE_SYMLINKS.md):
//! link targets are normalized and strip-prefixed like
//! `StripPrefixedPath.maybeDeprefixSymlink`, hard links are real hard links
//! created while streaming, and symbolic links are created only after every
//! other entry has been written. Special files, non-UTF-8 names, and semantic
//! tar extensions are deliberately unsupported. Strictly framed global PAX
//! comments are consumed as inert metadata. Like commons-compress
//! TarArchiveInputStream (used by CompressedTarFunction), GNU long names and
//! long link names (`L`/`K`) and local PAX headers (`x`) supply the next
//! entry's path/link name; other local PAX keys are accepted only when they are
//! inert for extraction (times, ownership, charset, comments, vendor
//! `SCHILY.`/`LIBARCHIVE.` attributes) or a `size` equal to the header size,
//! and anything else (sparse files, ...) is rejected. File permissions are normalized to 0755 when any executable bit is
//! set, or 0644 otherwise; directories use 0755. Ownership and privileged bits
//! are not preserved. Extraction never writes, links or creates a directory
//! through a symlink and never creates a hard link to a path outside the
//! extraction root. The caller must remove staging on failure and exclude
//! concurrent writers: metadata checks are not a defense against a hostile
//! concurrent rename.
//!
//! Extraction streams: archives are read from a file (or caller-owned bytes)
//! through incremental decoders (gzip, xz, zstd, bzip2) like Bazel's
//! CompressedTarFunction subclasses, and every entry is copied to disk in
//! constant memory. There are no artificial entry-count, expanded-size or
//! metadata-record caps (Bazel has none); the safety properties (no path
//! escape, no writes through symlinks, no hard links outside the root) are
//! enforced per entry, independent of size. The only size rule kept is
//! Bazel's own zip symlink precondition (`entry.getSize() < 256`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Cursor, Read, Seek};
use std::path::{Path, PathBuf};

// ZipDecompressor.MAX_PATH_LENGTH: Bazel requires `entry.getSize() < 256`.
const MAX_ZIP_LINK_TARGET: u64 = 256;
// CompressedTarFunction/CompressedFunction BUFFER_SIZE (32 KiB).
const BUFFER_SIZE: usize = 32 * 1024;
// GNU long-name/link and local PAX records are read whole into memory to
// name the next entry; real names are far below this. Larger records fail
// explicitly instead of allocating an attacker-chosen size.
const MAX_EXTENSION_RECORD: u64 = 1024 * 1024;
// TarZstFunction.java:33 `setLongMax(Zstd.windowLogMax())`: 2^31 on 64-bit.
const ZSTD_MAX_WINDOW: u64 = 1 << 31;

/// Archive types accepted by the native extractor (Bazel type names,
/// DecompressorValue.java:40-76). `gz`, `xz`, `zst`, `bz2`, `ar`, `deb` and
/// `7z` are not supported natively and fail explicitly in the callers.
pub const NATIVE_TYPES: &[&str] = &[
    "zip", "tar", "tar.gz", "tgz", "tar.xz", "txz", "tar.zst", "tzst", "tar.bz2", "tbz",
];

/// Extraction failure. `PrefixNotFound` carries Bazel's prefix suggestions
/// (`CouldNotFindPrefixException.maybeMakePrefixSuggestion`: the first segment
/// of every multi-segment entry seen before the prefix was found).
#[derive(Debug)]
pub enum Failure {
    PrefixNotFound(BTreeSet<String>),
    Other(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrefixNotFound(_) => f.write_str("archive strip_prefix did not match"),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self::Other(message.to_owned())
    }
}

/// Extract a verified archive without invoking external programs.
///
/// `destination` must be an existing, empty, private directory. Every entry is
/// checked before strip-prefix filtering, including entries outside the prefix.
pub fn extract(
    bytes: &[u8],
    archive_type: &str,
    strip_prefix: &str,
    destination: &Path,
) -> Result<(), String> {
    extract_named(bytes, archive_type, strip_prefix, destination, "archive")
}

/// Whether an extraction error is one of Bazel's own decompressor
/// IOException texts (the zip link-escape and missing hard-link target
/// errors), which `repository_ctx.extract` wraps like
/// DecompressorValue.decompress; other errors are native-profile limits.
pub fn is_bazel_extraction_io_error(error: &str) -> bool {
    error.starts_with(ZIP_LINK_ESCAPE)
        || (error.starts_with("File \"")
            && error.ends_with("\" does not exist")
            && error.contains("\" linked from \""))
}

const ZIP_LINK_ESCAPE: &str = "Zip entries cannot refer to files outside of their directory: ";

/// [`extract`] with the archive's display name, used where Bazel's own
/// diagnostics name the archive (the zip link-escape error).
pub fn extract_named(
    bytes: &[u8],
    archive_type: &str,
    strip_prefix: &str,
    destination: &Path,
    archive_name: &str,
) -> Result<(), String> {
    run(
        Cursor::new(bytes),
        archive_type,
        strip_prefix,
        destination,
        archive_name,
    )
    .map_err(|failure| match failure {
        Failure::PrefixNotFound(_) => {
            format!("archive strip_prefix {strip_prefix:?} did not match")
        }
        Failure::Other(message) => message,
    })
}

/// Stream-extract the archive file at `archive`, never reading it into
/// memory. Zip archives are read with random access (central directory).
pub fn extract_file(
    archive: &Path,
    archive_type: &str,
    strip_prefix: &str,
    destination: &Path,
    archive_name: &str,
) -> Result<(), Failure> {
    let file = File::open(archive).map_err(|e| format!("open archive: {e}"))?;
    run(file, archive_type, strip_prefix, destination, archive_name)
}

fn run<R: Read + Seek>(
    source: R,
    archive_type: &str,
    strip_prefix: &str,
    destination: &Path,
    archive_name: &str,
) -> Result<(), Failure> {
    let prefix = prefix_components(strip_prefix)?;
    check_directory(destination)?;
    if fs::read_dir(destination)
        .map_err(|e| format!("inspect extraction destination: {e}"))?
        .next()
        .is_some()
    {
        return Err("extraction destination must be empty".into());
    }
    let mut state = Extraction {
        destination,
        prefix,
        matched: strip_prefix.is_empty(),
        available: BTreeSet::new(),
        archive: archive_name,
        symlinks: BTreeMap::new(),
    };
    match archive_type {
        "tar" => extract_tar(BufReader::with_capacity(BUFFER_SIZE, source), &mut state)?,
        // GzipCompressorInputStream(in, decompressConcatenated = true).
        "tar.gz" | "tgz" => extract_tar(
            flate2::read::MultiGzDecoder::new(BufReader::with_capacity(BUFFER_SIZE, source)),
            &mut state,
        )?,
        // XZInputStream decodes concatenated streams (TarXzFunction.java).
        "tar.xz" | "txz" => extract_tar(
            lzma_rust2::XzReader::new(BufReader::with_capacity(BUFFER_SIZE, source), true),
            &mut state,
        )?,
        // ZstdInputStreamNoFinalizer: every frame, skippable frames skipped.
        "tar.zst" | "tzst" => extract_tar(
            ZstdFrames::new(BufReader::with_capacity(BUFFER_SIZE, source))?,
            &mut state,
        )?,
        // BZip2CompressorInputStream(in, decompressConcatenated = true).
        "tar.bz2" | "tbz" => {
            extract_tar(
                bzip2::read::MultiBzDecoder::new(BufReader::with_capacity(BUFFER_SIZE, source)),
                &mut state,
            )?;
        }
        "zip" => extract_zip(source, &mut state)?,
        other => return Err(format!("unsupported archive type: {other}").into()),
    }
    if !state.matched {
        return Err(Failure::PrefixNotFound(state.available));
    }
    Ok(state.create_symlinks(archive_type != "zip")?)
}

/// A zstd stream of one or more frames (RFC 8878 section 3.1), decoded
/// incrementally. Skippable frames are skipped like libzstd's streaming
/// decoder; the window is bounded like Bazel's `windowLogMax`.
struct ZstdFrames<R: io::BufRead> {
    decoder: Option<ruzstd::decoding::StreamingDecoder<R, ruzstd::decoding::FrameDecoder>>,
}

impl<R: io::BufRead> ZstdFrames<R> {
    fn new(source: R) -> Result<Self, String> {
        Ok(Self {
            decoder: Self::frame(source, ruzstd::decoding::FrameDecoder::new())?,
        })
    }

    fn frame(
        mut source: R,
        mut decoder: ruzstd::decoding::FrameDecoder,
    ) -> Result<Option<ruzstd::decoding::StreamingDecoder<R, ruzstd::decoding::FrameDecoder>>, String>
    {
        decoder.set_max_window_size(ZSTD_MAX_WINDOW);
        // Skippable frames (magic 0x184D2A5?, RFC 8878 section 3.1.2) carry no
        // content: skip them before the next data frame's header.
        loop {
            let mut header = [0u8; 8];
            let available = source
                .fill_buf()
                .map_err(|e| format!("read zstd stream: {e}"))?;
            if available.len() < 4
                || u32::from_le_bytes(available[..4].try_into().expect("4 bytes")) & !0xF
                    != 0x184D_2A50
            {
                break;
            }
            source
                .read_exact(&mut header)
                .map_err(|e| format!("truncated zstd skippable frame: {e}"))?;
            let length = u64::from(u32::from_le_bytes(header[4..].try_into().expect("4 bytes")));
            let skipped = io::copy(&mut (&mut source).take(length), &mut io::sink())
                .map_err(|e| format!("read zstd skippable frame: {e}"))?;
            if skipped != length {
                return Err("truncated zstd skippable frame".into());
            }
            if source
                .fill_buf()
                .map_err(|e| format!("read zstd stream: {e}"))?
                .is_empty()
            {
                return Ok(None);
            }
        }
        ruzstd::decoding::StreamingDecoder::new_with_decoder(source, decoder)
            .map(Some)
            .map_err(|e| format!("read zstd frame: {e}"))
    }
}

impl<R: io::BufRead> Read for ZstdFrames<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let Some(decoder) = self.decoder.as_mut() else {
                return Ok(0);
            };
            let count = decoder.read(buffer)?;
            if count != 0 || buffer.is_empty() {
                return Ok(count);
            }
            // Frame complete: continue with the next frame, if any.
            let (mut source, frame) = self.decoder.take().expect("decoder").into_parts();
            if source.fill_buf()?.is_empty() {
                return Ok(0);
            }
            self.decoder = Self::frame(source, frame).map_err(io::Error::other)?;
        }
    }
}

/// `PathFragment.create` normalization (UnixOsPathPolicy.normalize and
/// OsPathPolicy.removeRelativePaths): empty and `.` segments vanish, `..`
/// removes a preceding non-`..` segment, is dropped at an absolute root and is
/// otherwise kept, so relative paths may still start with `..`.
fn normalize(path: &str) -> (bool, Vec<&str>) {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            ".." if absolute => {}
            other => parts.push(other),
        }
    }
    (absolute, parts)
}

fn link_text(raw: &[u8]) -> Result<&str, String> {
    let text = std::str::from_utf8(raw).map_err(|_| "archive link target is not UTF-8")?;
    if text.is_empty() {
        return Err("archive link has an empty target".into());
    }
    if text.contains('\0') {
        return Err("archive link target contains NUL".into());
    }
    Ok(text)
}

fn prefix_components(prefix: &str) -> Result<Vec<&str>, String> {
    if prefix.is_empty() {
        return Ok(Vec::new());
    }
    let parts = safe_components(prefix.as_bytes())?;
    if parts.is_empty() || parts.join("/") != prefix {
        return Err("strip_prefix must contain only normal path components".into());
    }
    Ok(parts)
}

fn safe_components(raw: &[u8]) -> Result<Vec<&str>, String> {
    let path = std::str::from_utf8(raw).map_err(|_| "archive path is not UTF-8")?;
    if path.is_empty()
        || path.starts_with('/')
        || path.contains(['\\', '\0', ':'])
        || path.split('/').any(|part| part == "..")
    {
        return Err(format!("unsafe archive path: {path:?}"));
    }
    Ok(path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect())
}

struct Extraction<'a> {
    destination: &'a Path,
    prefix: Vec<&'a str>,
    matched: bool,
    /// Prefix suggestions collected while the prefix was not yet found.
    available: BTreeSet<String>,
    archive: &'a str,
    // Like Bazel's deferred `Map<Path, PathFragment> symlinks`, a later entry
    // for the same path replaces an earlier one. Keys are stripped components.
    symlinks: BTreeMap<Vec<String>, String>,
}

/// Output location of an included entry: absolute staging path and its
/// stripped, root-relative components.
type Output = (PathBuf, Vec<String>);

impl Extraction<'_> {
    fn output(&mut self, raw: &[u8], directory: bool) -> Result<Option<Output>, String> {
        let parts = safe_components(raw)?;
        if parts.is_empty() && !directory {
            return Err("archive file has an empty normalized path".into());
        }
        if !parts.starts_with(&self.prefix) {
            // CompressedTarFunction.java:96-99 / ZipDecompressor: suggestions
            // only until the prefix is first found.
            if !self.matched && parts.len() > 1 {
                self.available.insert(parts[0].to_owned());
            }
            return Ok(None);
        }
        let stripped = &parts[self.prefix.len()..];
        if stripped.is_empty() && !directory {
            return Err("strip_prefix resolves to a file rather than a directory".into());
        }
        self.matched = true;
        let mut output = self.destination.to_path_buf();
        for part in stripped {
            output.push(part);
        }
        Ok(Some((
            output,
            stripped.iter().map(|part| (*part).to_owned()).collect(),
        )))
    }

    /// `StripPrefixedPath.maybeDeprefixSymlink`: normalize, make relative,
    /// strip the prefix when the target starts with it (relative targets too),
    /// and report whether the raw target was absolute.
    fn deprefixed_target<'t>(&self, target: &'t str) -> (bool, Vec<&'t str>) {
        let (absolute, mut parts) = normalize(target);
        if !self.prefix.is_empty() && parts.starts_with(&self.prefix) {
            parts.drain(..self.prefix.len());
        }
        (absolute, parts)
    }

    /// Record a symbolic link for creation after all other entries, with the
    /// target text Bazel's repository ends up with. An absolute target is
    /// re-rooted at the extraction root (`root.getRelative(...)`) and then
    /// replanted by `RepositoryUtils.replantSymlinks` (RepositoryUtils.java:
    /// 160-164) as `"../" x depth` + the repository-relative target; native
    /// writes that relative text directly.
    fn symlink(&mut self, link: Vec<String>, raw: &str) -> Result<(), String> {
        let (absolute, parts) = self.deprefixed_target(raw);
        let mut target: Vec<&str> = Vec::new();
        if absolute {
            target.resize(link.len() - 1, "..");
        }
        target.extend(parts);
        let target = if target.is_empty() && absolute {
            ".".to_owned()
        } else {
            target.join("/")
        };
        if target.is_empty() {
            // Bazel would ask the OS for an empty-target symlink, which fails.
            return Err(format!(
                "archive symlink {:?} has an empty normalized target {raw:?}",
                link.join("/")
            ));
        }
        self.symlinks.insert(link, target);
        Ok(())
    }

    /// CompressedTarFunction hard links: `destinationPath().getRelative(target)`
    /// with the deprefixed target; a link to itself is dropped; an existing
    /// path is replaced. Bazel would link to a file outside the root for a
    /// relative target starting with `..`; native extraction refuses.
    fn hard_link(&self, path: &Path, link: &[String], raw: &str) -> Result<(), String> {
        let shown = link.join("/");
        let (_, parts) = self.deprefixed_target(raw);
        if parts.first() == Some(&"..") {
            return Err(format!(
                "archive hard link {shown:?} target {raw:?} escapes the extraction root (native extraction never links outside it)"
            ));
        }
        if parts.is_empty() {
            return Err(format!(
                "archive hard link {shown:?} targets the extraction root itself"
            ));
        }
        if parts.iter().copied().eq(link.iter().map(String::as_str)) {
            // CompressedTarFunction drops a hard link to itself.
            return Ok(());
        }
        // FileSystem.createHardLink (FileSystem.java:768-777): a target that does
        // not exist (missing, not yet extracted, outside the prefix, or below a
        // non-directory) fails with Bazel's exact text.
        let missing = || {
            format!(
                "File \"{}\" linked from \"{}\" does not exist",
                parts.last().expect("nonempty target"),
                link.last().expect("nonempty link")
            )
        };
        let mut target = self.destination.to_path_buf();
        for (index, part) in parts.iter().enumerate() {
            target.push(part);
            let metadata = match fs::symlink_metadata(&target) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(missing()),
                Err(error) => return Err(format!("inspect archive hard link target: {error}")),
            };
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "archive hard link {shown:?} target {raw:?} passes through a symlink"
                ));
            }
            if index + 1 < parts.len() && !metadata.is_dir() {
                return Err(missing());
            }
            if index + 1 == parts.len() && !metadata.is_file() {
                return Err(format!(
                    "archive hard link {shown:?} target {raw:?} is not a regular file (directory hard links are unsupported)"
                ));
            }
        }
        self.directory(path.parent().ok_or("archive hard link has no parent")?)?;
        replace_existing(path, &shown)?;
        fs::hard_link(&target, path)
            .map_err(|e| format!("create archive hard link {shown:?} to {raw:?}: {e}"))
    }

    /// Bazel creates symbolic links after the stream. Tar deletes an existing
    /// path first (CompressedTarFunction.java:160-166); zip calls
    /// `ensureSymbolicLink` directly (ZipDecompressor.java:118-120), whose
    /// `readSymbolicLink` of an existing regular file or directory throws
    /// `NotASymlinkException` (FileSystemUtils.java:329-349,
    /// UnixFileSystem.java:311-320, FileSystem.java:76-83): `<path> is not a
    /// symlink`. Bazel's map is unordered: a link below another link is either
    /// written through the first link or fails on a nonempty directory. Native
    /// extraction rejects that layout explicitly before creating any link.
    fn create_symlinks(&self, replace_existing_paths: bool) -> Result<(), String> {
        for link in self.symlinks.keys() {
            for depth in 1..link.len() {
                if self.symlinks.contains_key(&link[..depth]) {
                    return Err(format!(
                        "archive symlink {:?} is below archive symlink {:?} (native extraction never creates entries through a symlink)",
                        link.join("/"),
                        link[..depth].join("/")
                    ));
                }
            }
        }
        for (link, target) in &self.symlinks {
            let shown = link.join("/");
            let mut path = self.destination.to_path_buf();
            for part in link {
                path.push(part);
            }
            self.directory(path.parent().ok_or("archive symlink has no parent")?)?;
            if replace_existing_paths {
                replace_existing(&path, &shown)?;
            } else {
                match fs::symlink_metadata(&path) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!("inspect archive link path {shown:?}: {error}"));
                    }
                    Ok(_) => return Err(format!("{} is not a symlink", path.display())),
                }
            }
            create_symlink(target, &path)
                .map_err(|e| format!("create archive symlink {shown:?} -> {target:?}: {e}"))?;
        }
        Ok(())
    }

    fn directory(&self, path: &Path) -> Result<(), String> {
        check_directory(self.destination)?;
        let relative = path
            .strip_prefix(self.destination)
            .map_err(|_| "archive destination escaped staging")?;
        let mut current = self.destination.to_path_buf();
        for part in relative.components() {
            current.push(part);
            match fs::symlink_metadata(&current) {
                Ok(_) => check_directory(&current)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&current)
                        .map_err(|e| format!("create archive directory {current:?}: {e}"))?;
                    set_mode(&current, 0o755)?;
                }
                Err(e) => return Err(format!("inspect archive directory {current:?}: {e}")),
            }
        }
        Ok(())
    }

    fn file(&self, path: &Path, mode: u32, size: u64, reader: impl Read) -> Result<(), String> {
        self.directory(path.parent().ok_or("archive file has no parent")?)?;
        // Bazel writes every regular entry with `filePath.getOutputStream()`
        // (CompressedTarFunction.java:142-144, ZipDecompressor.java:168-170), a
        // truncating FileOutputStream (DiskBackedFileSystem.java:90-104): a later
        // entry for the same path wins, in place. Native extraction does the same
        // for an existing regular file (which can only be an earlier entry or an
        // in-root hard link, since no symlink exists before the final pass) and
        // never opens anything that is not a regular file.
        let existing = match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => true,
            Ok(metadata) if metadata.is_dir() => {
                return Err(format!(
                    "archive file {path:?} conflicts with an existing directory"
                ));
            }
            Ok(_) => {
                return Err(format!(
                    "archive file {path:?} conflicts with a symlink or special file"
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(format!("inspect archive file {path:?}: {error}")),
        };
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(!existing)
            .truncate(existing)
            .open(path)
            .map_err(|e| format!("create archive file {path:?}: {e}"))?;
        // The extra byte detects streams larger than the declared size without
        // allowing an attacker-controlled stream to expand without a bound.
        let copied = io::copy(&mut reader.take(size + 1), &mut output)
            .map_err(|e| format!("read archive file {path:?}: {e}"))?;
        if copied != size {
            return Err(format!(
                "archive file {path:?} has an incorrect expanded size"
            ));
        }
        set_file_mode(&output, if mode & 0o111 != 0 { 0o755 } else { 0o644 })
    }
}

/// `if (path.exists()) path.delete()` for a link entry: a regular file or an
/// empty directory is removed; a nonempty directory is an error, as Bazel's
/// delete fails. Nothing is followed (no symlink exists before the final pass).
fn replace_existing(path: &Path, shown: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("inspect archive link path {shown:?}: {error}")),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("archive link path {shown:?} is already a symlink"))
        }
        Ok(metadata) if metadata.is_dir() => fs::remove_dir(path).map_err(|e| {
            format!("archive link {shown:?} conflicts with a directory that cannot be deleted: {e}")
        }),
        Ok(metadata) if metadata.is_file() => fs::remove_file(path)
            .map_err(|e| format!("replace archive file with link {shown:?}: {e}")),
        Ok(_) => Err(format!("archive link path {shown:?} is a special file")),
    }
}

#[cfg(unix)]
fn create_symlink(target: &str, path: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(not(unix))]
fn create_symlink(_target: &str, _path: &Path) -> io::Result<()> {
    Err(io::Error::other(
        "archive symlinks are supported only on Unix",
    ))
}

fn check_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("inspect archive directory {path:?}: {e}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("archive directory conflict or symlink: {path:?}"));
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|e| format!("set archive directory permissions: {e}"))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

fn set_file_mode(file: &File, mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|e| format!("set archive file permissions: {e}"))?;
    }
    #[cfg(not(unix))]
    let _ = (file, mode);
    Ok(())
}

fn validate_tar_string(field: &[u8]) -> Result<(), String> {
    if let Some(end) = field.iter().position(|byte| *byte == 0)
        && field[end..].iter().any(|byte| *byte != 0)
    {
        return Err("malformed tar path or link name: embedded NUL".into());
    }
    Ok(())
}

/// Consume only inert global comments without allocating metadata-sized buffers.
/// Values are opaque bytes: embedded newlines, NULs and non-UTF-8 bytes have no
/// extraction semantics. Framing is byte-length-delimited, never line-delimited.
fn consume_global_comments(
    reader: impl Read,
    size: u64,
    records: &mut usize,
) -> Result<(), String> {
    let mut reader = BufReader::with_capacity(8192, reader.take(size));
    let mut remaining = size;
    while remaining != 0 {
        *records += 1;
        let mut length = 0u64;
        let mut prefix = 0u64;
        loop {
            if prefix == remaining {
                return Err("invalid PAX record length framing".into());
            }
            let mut byte = [0];
            reader
                .read_exact(&mut byte)
                .map_err(|e| format!("truncated PAX record: {e}"))?;
            prefix += 1;
            if byte[0] == b' ' && prefix > 1 {
                break;
            }
            if !byte[0].is_ascii_digit() {
                return Err("invalid PAX record length framing".into());
            }
            length = length
                .checked_mul(10)
                .and_then(|n| n.checked_add(u64::from(byte[0] - b'0')))
                .ok_or("PAX record length overflow")?;
        }
        // Eight bytes for `comment=` and one terminal LF, beyond the prefix.
        let value_size = length
            .checked_sub(prefix + 9)
            .filter(|_| length <= remaining)
            .ok_or("invalid PAX record length")?;
        let mut key = [0; 8];
        reader
            .read_exact(&mut key)
            .map_err(|e| format!("truncated PAX record key: {e}"))?;
        if &key != b"comment=" {
            return Err("unsupported global PAX key; only comment is supported".into());
        }
        let copied = io::copy(&mut reader.by_ref().take(value_size), &mut io::sink())
            .map_err(|e| format!("read PAX comment: {e}"))?;
        if copied != value_size {
            return Err("truncated PAX comment value".into());
        }
        let mut newline = [0];
        reader
            .read_exact(&mut newline)
            .map_err(|e| format!("truncated PAX record ending: {e}"))?;
        if newline[0] != b'\n' {
            return Err("PAX record must end with a newline".into());
        }
        remaining -= length;
    }
    Ok(())
}

fn extract_tar(reader: impl Read, state: &mut Extraction<'_>) -> Result<(), String> {
    let mut archive = tar::Archive::new(reader);
    // Raw iteration prevents the library from buffering arbitrarily large GNU
    // and PAX metadata records, or silently ignoring their unsafe paths.
    let entries = archive
        .entries()
        .map_err(|e| format!("read tar archive: {e}"))?
        .raw(true);
    let mut pax_records = 0;
    let mut pending_global = false;
    let mut long_name: Option<Vec<u8>> = None;
    let mut long_link: Option<Vec<u8>> = None;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("read tar entry: {e}"))?;
        let header = entry.header();
        let raw = header.as_bytes();
        validate_tar_string(&raw[..100])?;
        validate_tar_string(&raw[157..257])?;
        if &raw[257..263] == b"ustar\0" {
            validate_tar_string(&raw[345..500])?;
        }
        let kind = header.entry_type();
        let directory = kind.is_dir();
        let global = kind == tar::EntryType::XGlobalHeader;
        // TarArchiveEntry.isSymbolicLink() / isLink(): ustar types '2' and '1'.
        let symlink = kind == tar::EntryType::Symlink;
        let hard_link = kind == tar::EntryType::Link;
        let gnu_name = kind == tar::EntryType::GNULongName;
        let gnu_link = kind == tar::EntryType::GNULongLink;
        if gnu_name || gnu_link || kind == tar::EntryType::XHeader {
            // Extension records describe the next entry (commons-compress
            // getNextTarEntry: GNU long names/links and local PAX headers).
            let size = entry.size();
            if size > MAX_EXTENSION_RECORD {
                return Err(format!(
                    "tar extension record ({kind:?}) of {size} bytes exceeds the native 1 MiB limit"
                ));
            }
            let mut data = Vec::new();
            (&mut entry)
                .take(size)
                .read_to_end(&mut data)
                .map_err(|e| format!("read tar extension record: {e}"))?;
            if data.len() as u64 != size {
                return Err("truncated tar extension record".into());
            }
            if gnu_name || gnu_link {
                // TarArchiveInputStream.getLongNameData: trailing NULs dropped.
                while data.last() == Some(&0) {
                    data.pop();
                }
                if data.is_empty() || data.contains(&0) {
                    return Err("malformed GNU long name record".into());
                }
                if gnu_name {
                    long_name = Some(data);
                } else {
                    long_link = Some(data);
                }
            } else {
                let (path, link) = local_pax(&data, &mut pax_records)?;
                if let Some(path) = path {
                    long_name = Some(path);
                }
                if let Some(link) = link {
                    long_link = Some(link);
                }
            }
            continue;
        }
        // POSIX type '7' (contiguous file) is an ordinary regular file for an
        // extractor, as in commons-compress (TarArchiveEntry.isFile) and GNU tar.
        let regular = kind.is_file() || kind.is_contiguous();
        if !directory && !regular && !global && !symlink && !hard_link {
            return Err(format!(
                "unsupported tar entry type (special files and extension records are rejected): {kind:?}"
            ));
        }
        if global && (long_name.is_some() || long_link.is_some()) {
            return Err("tar extension record precedes a global PAX header".into());
        }
        let path_bytes = match long_name.take() {
            Some(name) => name,
            None => entry.path_bytes().into_owned(),
        };
        let link_target = match long_link.take() {
            Some(link) => Some(link),
            None => entry.link_name_bytes().map(|name| name.into_owned()),
        };
        if (symlink || hard_link) != link_target.is_some() {
            return Err(if link_target.is_some() {
                "tar link names are unsupported on regular, directory and metadata entries".into()
            } else {
                "tar link entry has an empty link name".into()
            });
        }
        let link_target = link_target.as_deref().map(link_text).transpose()?;
        let size = entry.size();
        if directory && size != 0 {
            return Err("tar directory has nonempty file data".into());
        }
        let mode = header
            .mode()
            .map_err(|e| format!("invalid tar mode: {e}"))?;
        if global {
            // A metadata name is never an output or a strip-prefix match. Keep
            // the native safe-relative-path restriction even for these names.
            if safe_components(&path_bytes)?.is_empty() {
                return Err("global PAX header requires a nonempty safe path".into());
            }
            consume_global_comments(&mut entry, size, &mut pax_records)?;
            pending_global = true;
            continue;
        }
        let output = state.output(&path_bytes, directory)?;
        if let Some((output, parts)) = output {
            if directory {
                state.directory(&output)?;
            } else if let Some(target) = link_target {
                // CompressedTarFunction.java:110 creates the parent first.
                state.directory(output.parent().ok_or("archive link has no parent")?)?;
                if symlink {
                    state.symlink(parts, target)?;
                } else {
                    state.hard_link(&output, &parts, target)?;
                }
            } else {
                state.file(&output, mode, size, &mut entry)?;
            }
        }
        pending_global = false;
    }
    // Read remaining gzip members and validate their checksums. Tar permits
    // zero padding after its end marker; reject nonzero trailing data instead
    // of silently accepting a second archive or hidden entries.
    let mut reader = archive.into_inner();
    let mut buffer = [0; 8192];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|e| format!("read tar trailer: {e}"))?;
        if count == 0 {
            break;
        }
        if buffer[..count].iter().any(|byte| *byte != 0) {
            return Err("nonzero trailing data after tar end marker".into());
        }
    }
    if pending_global {
        return Err("global PAX header has no following regular file or directory".into());
    }
    if long_name.is_some() || long_link.is_some() {
        return Err("tar extension record has no following entry".into());
    }
    Ok(())
}

/// Path and link-name overrides from a local PAX header.
type PaxNames = (Option<Vec<u8>>, Option<Vec<u8>>);

/// Parse a local PAX extended header (`length key=value\n` records) into the
/// overriding path and link name. Keys without extraction semantics are
/// ignored like commons-compress; semantic keys native extraction does not
/// implement (sparse files, ...) fail explicitly.
fn local_pax(data: &[u8], records: &mut usize) -> Result<PaxNames, String> {
    let mut path = None;
    let mut link = None;
    let mut rest = data;
    while !rest.is_empty() {
        *records += 1;
        let space = rest
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or("invalid PAX record length framing")?;
        let digits = &rest[..space];
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return Err("invalid PAX record length framing".into());
        }
        let length: usize = std::str::from_utf8(digits)
            .expect("ASCII digits")
            .parse()
            .map_err(|_| "PAX record length overflow")?;
        if length <= space + 1 || length > rest.len() || rest[length - 1] != b'\n' {
            return Err("invalid PAX record length".into());
        }
        let record = &rest[space + 1..length - 1];
        let equals = record
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or("PAX record has no '='")?;
        let key = std::str::from_utf8(&record[..equals]).map_err(|_| "non-UTF-8 PAX key")?;
        let value = &record[equals + 1..];
        match key {
            "path" => path = Some(value.to_vec()),
            "linkpath" => link = Some(value.to_vec()),
            "size" => {
                // The header size governs framing natively; PAX sizes (files
                // of 8 GiB and more) are unsupported, never silently ignored.
                return Err("PAX size records are unsupported by native extraction".into());
            }
            "mtime" | "atime" | "ctime" | "uid" | "gid" | "uname" | "gname" | "comment"
            | "charset" | "hdrcharset" => {}
            key if key.starts_with("SCHILY.") || key.starts_with("LIBARCHIVE.") => {
                if key.starts_with("SCHILY.devm") || key == "SCHILY.filetype" {
                    return Err(format!("unsupported PAX key {key:?}"));
                }
            }
            key => return Err(format!("unsupported PAX key {key:?}")),
        }
        rest = &rest[length..];
    }
    Ok((path, link))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ZipKind {
    Directory,
    File,
    Symlink,
}

fn zip_entry_info<R: Read + ?Sized>(
    entry: &zip::read::ZipFile<'_, R>,
) -> Result<(ZipKind, u32), String> {
    safe_components(entry.name_raw())?;
    let directory = entry.is_dir();
    let mode = entry.unix_mode().unwrap_or(0o644);
    let kind = match mode & 0o170000 {
        0 | 0o040000 if directory => ZipKind::Directory,
        0 | 0o100000 if !directory => ZipKind::File,
        // ZipDecompressor S_IFLNK: the entry data is the link target.
        0o120000 if !directory => ZipKind::Symlink,
        _ => {
            return Err("unsupported zip entry type (special files are rejected)".into());
        }
    };
    if directory && entry.size() != 0 {
        return Err("zip directory has nonempty file data".into());
    }
    if kind == ZipKind::Symlink && entry.size() >= MAX_ZIP_LINK_TARGET {
        // Bazel's Preconditions.checkState(entry.getSize() < MAX_PATH_LENGTH).
        return Err(format!(
            "zip symlink target must be shorter than {MAX_ZIP_LINK_TARGET} bytes"
        ));
    }
    Ok((kind, mode))
}

/// ZipDecompressor's escape check on the stripped link path and the
/// normalized (not yet deprefixed) target; absolute targets are not checked.
fn zip_link_escapes(link: &[String], target: &str) -> bool {
    let (absolute, parts) = normalize(target);
    if absolute || parts.first() != Some(&"..") {
        return false;
    }
    let mut points: Vec<&str> = link[..link.len() - 1].iter().map(String::as_str).collect();
    for part in parts {
        if part == ".." && points.last().is_some_and(|last| *last != "..") {
            points.pop();
        } else {
            points.push(part);
        }
    }
    points.first() == Some(&"..")
}

fn extract_zip<R: Read + Seek>(source: R, state: &mut Extraction<'_>) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(source).map_err(|e| format!("read zip archive: {e}"))?;
    // Like Bazel's ZipReader (ZipFileData.java:39, 54, 203-204: a LinkedHashMap
    // keyed by name), ZipArchive keeps one record per name: the later record's
    // data at the first record's position.
    // Preflight every path, type and declared size, even outside strip_prefix.
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|e| format!("read zip entry: {e}"))?;
        zip_entry_info(&entry)?;
    }
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("read zip entry: {e}"))?;
        let (kind, mode) = zip_entry_info(&entry)?;
        let directory = kind == ZipKind::Directory;
        let output = state.output(entry.name_raw(), directory)?;
        if let Some((output, parts)) = &output {
            match kind {
                ZipKind::Directory => state.directory(output)?,
                ZipKind::File => state.file(output, mode, entry.size(), &mut entry)?,
                ZipKind::Symlink => {
                    let size = entry.size();
                    let mut raw = Vec::new();
                    (&mut entry)
                        .take(size + 1)
                        .read_to_end(&mut raw)
                        .map_err(|e| format!("read zip symlink target: {e}"))?;
                    if raw.len() as u64 != size {
                        return Err("zip symlink has an incorrect expanded size".into());
                    }
                    let target = link_text(&raw)?;
                    if zip_link_escapes(parts, target) {
                        // ZipDecompressor.java:152-164, verbatim.
                        return Err(format!(
                            "{ZIP_LINK_ESCAPE}{} has a symlink {} pointing to {target}",
                            state.archive,
                            parts.join("/")
                        ));
                    }
                    state.directory(output.parent().ok_or("archive link has no parent")?)?;
                    state.symlink(parts.clone(), target)?;
                }
            }
        }
        if directory || output.is_none() {
            // ZIP validates CRC on reading, not dropping. Validate skipped files
            // and empty directory payloads too, bounded by their declared size.
            let size = entry.size();
            let read = io::copy(&mut entry.take(size + 1), &mut io::sink())
                .map_err(|e| format!("read skipped zip entry: {e}"))?;
            if read != size {
                return Err("skipped zip entry has incorrect expanded size".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tar_fixture(entries: &[(&str, u8, u32, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, kind, mode, data) in entries {
            let mut header = tar::Header::new_ustar();
            header.set_mode(*mode);
            header.set_size(data.len() as u64);
            header.set_entry_type(tar::EntryType::new(*kind));
            // Deliberately bypass builder path sanitization for adversarial fixtures.
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            if *kind == b'1' || *kind == b'2' {
                header.as_mut_bytes()[157..167].copy_from_slice(b"../outside");
            }
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn zip_fixture(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, mode, data) in entries {
            let options = zip::write::SimpleFileOptions::default().unix_permissions(*mode);
            if name.ends_with('/') {
                writer.add_directory(*name, options).unwrap();
            } else {
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
        }
        writer.finish().unwrap().into_inner()
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut writer = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        writer.write_all(bytes).unwrap();
        writer.finish().unwrap()
    }

    fn run(bytes: &[u8], kind: &str, prefix: &str) -> Result<tempfile::TempDir, String> {
        let destination = tempfile::tempdir().unwrap();
        extract(bytes, kind, prefix, destination.path())?;
        Ok(destination)
    }

    #[test]
    fn binary_and_executable_modes_for_all_formats() {
        let binary = b"\0\xff\x80\n\r\0";
        let tar = tar_fixture(&[
            ("./root/", b'5', 0o700, b""),
            ("./root/bin/tool", b'0', 0o6711, binary),
            ("root/data", b'0', 0o600, binary),
        ]);
        let zip = zip_fixture(&[
            ("root/bin/tool", 0o711, binary),
            ("root/data", 0o600, binary),
        ]);
        for (kind, bytes) in [
            ("tar", tar.clone()),
            ("tar.gz", gzip(&tar)),
            ("tgz", gzip(&tar)),
            ("zip", zip),
        ] {
            let destination = run(&bytes, kind, "root").unwrap();
            assert_eq!(
                fs::read(destination.path().join("bin/tool")).unwrap(),
                binary
            );
            assert_eq!(fs::read(destination.path().join("data")).unwrap(), binary);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(destination.path().join("bin/tool"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o7777,
                    0o755
                );
                assert_eq!(
                    fs::metadata(destination.path().join("data"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o7777,
                    0o644
                );
            }
        }
    }

    #[test]
    fn prefix_matches_components_and_directory_only_matches_count() {
        for (kind, bytes) in [
            (
                "tar",
                tar_fixture(&[
                    ("rooted/no", b'0', 0o644, b"no"),
                    ("root/", b'5', 0o755, b""),
                ]),
            ),
            (
                "zip",
                zip_fixture(&[("rooted/no", 0o644, b"no"), ("root/", 0o755, b"")]),
            ),
        ] {
            let destination = run(&bytes, kind, "root").unwrap();
            assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 0);
            assert!(
                run(&bytes, kind, "missing")
                    .unwrap_err()
                    .contains("did not match")
            );
            assert!(
                run(&bytes, kind, "")
                    .unwrap()
                    .path()
                    .join("rooted/no")
                    .is_file()
            );
        }
        let empty = tar_fixture(&[]);
        assert!(run(&empty, "tar", "").is_ok());
        assert!(run(&empty, "tar", "missing").is_err());
        assert!(run(&zip_fixture(&[]), "zip", "").is_ok());
    }

    #[test]
    fn unsafe_paths_are_rejected_before_filtering() {
        for name in [
            "../evil",
            "other/../evil",
            "/absolute",
            "C:/drive",
            "C:drive",
            "other\\evil",
            "//server/path",
            "other/a\0hidden",
        ] {
            let tar = tar_fixture(&[(name, b'0', 0o644, b"bad")]);
            assert!(run(&tar, "tar", "wanted").is_err(), "tar accepted {name:?}");
            let zip = zip_fixture(&[(name, 0o644, b"bad")]);
            assert!(run(&zip, "zip", "wanted").is_err(), "zip accepted {name:?}");
        }
    }

    #[test]
    fn invalid_prefixes_and_file_prefixes_are_rejected() {
        let bytes = tar_fixture(&[("root", b'0', 0o644, b"data")]);
        for prefix in [
            ".",
            "./root",
            "root/",
            "root//child",
            "..",
            "root/../child",
            "/root",
            "C:root",
            "a\\b",
            "a\0b",
            "root",
        ] {
            assert!(run(&bytes, "tar", prefix).is_err(), "accepted {prefix:?}");
        }
    }

    #[test]
    fn global_comments_are_inert_and_do_not_become_output_entries() {
        let bytes = tar_fixture(&[
            ("metadata", b'g', 0o644, b"16 comment=seed\n"),
            ("pkg/file", b'0', 0o644, b"hello"),
        ]);
        let destination = tempfile::tempdir().unwrap();
        extract(&bytes, "tar", "pkg", destination.path()).unwrap();
        assert_eq!(fs::read(destination.path().join("file")).unwrap(), b"hello");
        assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 1);
    }

    #[test]
    fn tar_special_files_and_extensions_are_explicit_errors() {
        for kind in *b"346S" {
            let bytes = tar_fixture(&[("unrelated", kind, 0o644, b"")]);
            assert!(
                run(&bytes, "tar", "wanted")
                    .unwrap_err()
                    .contains("unsupported tar entry type"),
                "type {kind}"
            );
        }
    }

    fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
        let mut length = key.len() + value.len() + 3;
        loop {
            let actual = length.to_string().len() + key.len() + value.len() + 3;
            if actual == length {
                break;
            }
            length = actual;
        }
        let mut record = format!("{length} {key}=").into_bytes();
        record.extend_from_slice(value);
        record.push(b'\n');
        assert_eq!(record.len(), length);
        record
    }

    #[test]
    fn global_comments_use_byte_lengths_and_accept_opaque_values() {
        for value in [
            b"".as_slice(),
            b"../not-a-path",
            b"line\nnext=one\0\xff",
            "日本語".as_bytes(),
            &vec![b'x'; 8193],
        ] {
            let record = pax_record("comment", value);
            let mut records = 0;
            consume_global_comments(Cursor::new(&record), record.len() as u64, &mut records)
                .unwrap();
            assert_eq!(records, 1);
        }
        let data = b"0014 comment=\n12 comment=\n";
        let mut records = 0;
        consume_global_comments(Cursor::new(data), data.len() as u64, &mut records).unwrap();
        assert_eq!(records, 2);
    }

    #[test]
    fn malformed_or_semantic_global_metadata_is_rejected_before_filtering() {
        for payload in [
            b"0".as_slice(),
            b"0 comment=\n",
            b"-1 comment=\n",
            b"+12 comment=\n",
            b"a comment=\n",
            b" comment=\n",
            b"12comment=\n",
            b"12 comment= ",
            b"11 comment=\n",
            b"13 comment=\n",
            b"18446744073709551616 comment=\n",
            b"12 comment=\nJUNK",
            b"12 comment:\n",
        ] {
            let bytes = tar_fixture(&[
                ("unrelated/meta", b'g', 0o644, payload),
                ("wanted/f", b'0', 0o644, b"data"),
            ]);
            assert!(
                run(&bytes, "tar", "wanted").unwrap_err().contains("PAX"),
                "{payload:?}"
            );
        }
        for key in [
            "path",
            "linkpath",
            "size",
            "mtime",
            "uid",
            "GNU.sparse.map",
            "SCHILY.archtype",
            "unknown",
            "commentary",
            "",
        ] {
            let payload = pax_record(key, b"../../outside-and-some-padding");
            let bytes = tar_fixture(&[
                ("unrelated/meta", b'g', 0o644, &payload),
                ("wanted/f", b'0', 0o644, b"data"),
            ]);
            assert!(
                run(&bytes, "tar", "wanted")
                    .unwrap_err()
                    .contains("unsupported global PAX key"),
                "{key}"
            );
        }
        for end in 0..16 {
            let mut records = 0;
            assert!(
                consume_global_comments(
                    Cursor::new(&b"16 comment=seed\n"[..end]),
                    16,
                    &mut records
                )
                .is_err()
            );
        }
    }

    #[test]
    fn global_metadata_requires_a_real_following_member_and_never_matches_prefix() {
        let comment = b"12 comment=\n";
        assert!(
            run(&tar_fixture(&[("meta", b'g', 0o644, comment)]), "tar", "")
                .unwrap_err()
                .contains("no following")
        );
        assert!(
            run(
                &tar_fixture(&[("f", b'0', 0o644, b"data"), ("meta", b'g', 0o644, comment)]),
                "tar",
                ""
            )
            .unwrap_err()
            .contains("no following")
        );
        // Empty and consecutive globals are valid when a real entry follows.
        let bytes = tar_fixture(&[
            ("wanted", b'g', 0o644, b""),
            ("meta", b'g', 0o644, comment),
            ("other/f", b'0', 0o644, b"data"),
        ]);
        run(&bytes, "tar", "").unwrap();
        // A filtered real member clears the trailing-global guard, but metadata
        // itself must not satisfy the requested prefix.
        assert!(
            run(&bytes, "tar", "wanted")
                .unwrap_err()
                .contains("did not match")
        );
        run(
            &tar_fixture(&[
                ("wanted/f", b'0', 0o644, b"data"),
                ("meta", b'g', 0o644, b""),
                ("other/f", b'0', 0o644, b"other"),
            ]),
            "tar",
            "wanted",
        )
        .unwrap();
        run(
            &tar_fixture(&[("meta", b'g', 0o644, b""), ("directory/", b'5', 0o755, b"")]),
            "tar",
            "",
        )
        .unwrap();
    }

    #[test]
    fn global_metadata_retains_header_path_link_mode_and_checksum_guards() {
        for name in ["../outside", "/absolute", "C:drive", "a\\b", "", "."] {
            let bytes = tar_fixture(&[
                (name, b'g', 0o644, b"12 comment=\n"),
                ("wanted/f", b'0', 0o644, b"data"),
            ]);
            assert!(run(&bytes, "tar", "wanted").is_err(), "{name}");
        }
        for index in [0, 100, 157] {
            let mut bytes = tar_fixture(&[
                ("meta", b'g', 0o644, b"12 comment=\n"),
                ("f", b'0', 0o644, b"data"),
            ]);
            bytes[index] = if index == 100 { b'Z' } else { b'x' };
            if index != 0 {
                let mut header = tar::Header::new_ustar();
                header.as_mut_bytes().copy_from_slice(&bytes[..512]);
                header.set_cksum();
                bytes[..512].copy_from_slice(header.as_bytes());
            }
            assert!(run(&bytes, "tar", "").is_err(), "header offset {index}");
        }
    }

    #[test]
    fn global_record_budget_spans_headers_and_values_stream_without_large_reads() {
        struct SmallReads<R>(R);
        impl<R: Read> Read for SmallReads<R> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                assert!(
                    output.len() <= 8192,
                    "metadata read allocated an oversized buffer"
                );
                self.0.read(output)
            }
        }
        let record = pax_record("comment", &vec![b'x'; 1024 * 1024]);
        let mut records = 0;
        consume_global_comments(
            SmallReads(Cursor::new(&record)),
            record.len() as u64,
            &mut records,
        )
        .unwrap();
        assert_eq!(records, 1);
        consume_global_comments(Cursor::new(b""), 0, &mut records).unwrap();
        assert_eq!(records, 1);
        // No record-count cap (Bazel has none): the former 100000-record
        // profile limit is gone, records still stream in small reads.
        let first = b"12 comment=\n".repeat(50_000);
        let second = b"12 comment=\n".repeat(50_001);
        let bytes = tar_fixture(&[
            ("first", b'g', 0o644, &first),
            ("second", b'g', 0o644, &second),
            ("f", b'0', 0o644, b"data"),
        ]);
        let destination = run(&bytes, "tar", "").unwrap();
        assert_eq!(fs::read(destination.path().join("f")).unwrap(), b"data");
    }

    #[test]
    fn global_comments_preserve_gzip_checksums_and_hidden_trailer_rejection() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let bytes = tar_fixture(&[
            ("meta", b'g', 0o644, b"12 comment=\n"),
            ("f", b'0', 0o644, b"data"),
        ]);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&bytes).unwrap();
        let mut compressed = encoder.finish().unwrap();
        run(&compressed, "tar.gz", "").unwrap();
        let crc = compressed.len() - 8;
        compressed[crc] ^= 1;
        assert!(run(&compressed, "tar.gz", "").is_err());
        let mut hidden = bytes;
        hidden.extend_from_slice(&tar_fixture(&[("hidden", b'0', 0o644, b"bad")]));
        assert!(
            run(&hidden, "tar", "")
                .unwrap_err()
                .contains("trailing data")
        );
    }

    #[test]
    fn zip_special_files_are_rejected_before_filtering() {
        for kind in [0o010000u32, 0o060000, 0o140000] {
            let mut bytes = zip_fixture(&[("unrelated", 0o644, b"target")]);
            let central = bytes
                .windows(4)
                .position(|part| part == b"PK\x01\x02")
                .unwrap();
            bytes[central + 5] = 3; // Unix creator.
            bytes[central + 38..central + 42]
                .copy_from_slice(&((kind | 0o644) << 16).to_le_bytes());
            assert!(
                run(&bytes, "zip", "wanted")
                    .unwrap_err()
                    .contains("unsupported zip entry type")
            );
        }
    }

    #[test]
    fn directory_duplicates_and_later_files_win_but_kind_conflicts_are_rejected() {
        let good = tar_fixture(&[
            ("dir/", b'5', 0o755, b""),
            ("dir/", b'5', 0o755, b""),
            ("dir/file", b'0', 0o644, b"ok"),
        ]);
        assert!(run(&good, "tar", "").is_ok());
        // Bazel's truncating getOutputStream: the later regular entry wins.
        let later = [
            ("file", 0o644, &b"one, longer"[..]),
            ("./file", 0o755, &b"two"[..]),
        ];
        let tar = tar_fixture(&later.map(|(name, mode, data)| (name, b'0', mode, data)));
        for (kind, bytes) in [("tar", tar), ("zip", zip_fixture(&later))] {
            let destination = run(&bytes, kind, "").unwrap();
            assert_eq!(fs::read(destination.path().join("file")).unwrap(), b"two");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(destination.path().join("file"))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o755, "{kind}");
            }
        }
        for entries in [
            vec![
                ("dir", b'0', 0o644, &b"one"[..]),
                ("dir/file", b'0', 0o644, &b"two"[..]),
            ],
            vec![
                ("dir/", b'5', 0o755, &b""[..]),
                ("dir", b'0', 0o644, &b"two"[..]),
            ],
            vec![
                ("dir/file", b'0', 0o644, &b"one"[..]),
                ("dir", b'0', 0o644, &b"two"[..]),
            ],
        ] {
            assert!(run(&tar_fixture(&entries), "tar", "").is_err());
            let zip_entries: Vec<_> = entries
                .iter()
                .map(|(name, _, mode, data)| (*name, *mode, *data))
                .collect();
            assert!(run(&zip_fixture(&zip_entries), "zip", "").is_err());
        }
    }

    #[test]
    fn gzip_concatenated_members_and_corrupt_trailers() {
        let tar = tar_fixture(&[("file", b'0', 0o644, b"binary\0data")]);
        let mut concatenated = gzip(&tar[..600]);
        concatenated.extend(gzip(&tar[600..]));
        assert!(run(&concatenated, "tar.gz", "").is_ok());
        let last = concatenated.len() - 8;
        concatenated[last] ^= 1;
        assert!(run(&concatenated, "tar.gz", "").is_err());
        let mut double = gzip(&tar);
        double.extend(gzip(&tar));
        assert!(
            run(&double, "tar.gz", "")
                .unwrap_err()
                .contains("trailing data")
        );
    }

    #[test]
    fn reviewer_skipped_zip_files_and_directories_validate_crc_and_size() {
        for directory in [false, true] {
            let skipped = if directory { "other/" } else { "other/file" };
            let payload: &[u8] = if directory { b"" } else { b"skipped bytes" };
            let original =
                zip_fixture(&[("wanted/file", 0o644, b"valid"), (skipped, 0o644, payload)]);
            let central = original
                .windows(4)
                .enumerate()
                .filter(|(_, b)| *b == b"PK\x01\x02")
                .nth(1)
                .unwrap()
                .0;
            let mut bad_crc = original.clone();
            bad_crc[central + 16] ^= 1;
            assert!(
                run(&bad_crc, "zip", "wanted").is_err(),
                "skipped CRC was not checked, directory={directory}"
            );
            if !directory {
                let mut wrong_size = original;
                wrong_size[central + 24..central + 28]
                    .copy_from_slice(&((payload.len() + 1) as u32).to_le_bytes());
                assert!(
                    run(&wrong_size, "zip", "wanted").is_err(),
                    "skipped payload size was not checked"
                );
            }
        }
    }

    #[test]
    fn malformed_and_oversized_inputs_are_rejected() {
        assert!(run(b"bad", "tar", "").is_err());
        assert!(run(b"bad", "tar.gz", "").is_err());
        assert!(run(b"bad", "zip", "").is_err());
        assert!(
            run(b"", "rar", "")
                .unwrap_err()
                .contains("unsupported archive type")
        );
        // A declared size beyond the actual data is a truncated archive, not
        // a size-cap failure (there is no expanded-size cap).
        let mut header = tar::Header::new_ustar();
        header.set_path("outside-prefix").unwrap();
        header.set_mode(0o644);
        header.set_size(2 * 1024 * 1024 * 1024);
        header.set_cksum();
        let error = run(header.as_bytes(), "tar", "wanted").unwrap_err();
        assert!(!error.contains("limit"), "{error}");
        for kind in ["tar.xz", "txz", "tar.zst", "tzst", "tar.bz2", "tbz"] {
            assert!(run(b"bad", kind, "").is_err(), "{kind}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn existing_symlinks_are_never_followed() {
        use std::os::unix::fs::symlink;
        let destination = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), destination.path().join("link")).unwrap();
        let state = Extraction {
            destination: destination.path(),
            prefix: vec![],
            matched: true,
            available: BTreeSet::new(),
            archive: "archive",
            symlinks: BTreeMap::new(),
        };
        assert!(
            state
                .directory(&destination.path().join("link/subdir"))
                .is_err()
        );
        assert!(!outside.path().join("subdir").exists());
        let bytes = tar_fixture(&[("file", b'0', 0o644, b"bad")]);
        assert!(extract(&bytes, "tar", "", &destination.path().join("link")).is_err());
    }

    type LinkEntry<'a> = (&'a str, u8, &'a str, &'a [u8]);

    fn tar_links(entries: &[LinkEntry<'_>]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, kind, link, data) in entries {
            let mut header = tar::Header::new_ustar();
            header.set_mode(0o644);
            header.set_size(data.len() as u64);
            header.set_entry_type(tar::EntryType::new(*kind));
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.as_mut_bytes()[157..157 + link.len()].copy_from_slice(link.as_bytes());
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    /// `(name, Some(target))` is a symlink; `(name, None)` a file or directory.
    fn zip_links(entries: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, link, data) in entries {
            let options = zip::write::SimpleFileOptions::default().unix_permissions(0o644);
            match link {
                Some(target) => writer.add_symlink(*name, *target, options).unwrap(),
                None if name.ends_with('/') => writer.add_directory(*name, options).unwrap(),
                None => {
                    writer.start_file(*name, options).unwrap();
                    writer.write_all(data).unwrap();
                }
            }
        }
        writer.finish().unwrap().into_inner()
    }

    fn tar_formats(tar: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("tar", tar.to_vec()),
            ("tar.gz", gzip(tar)),
            ("tgz", gzip(tar)),
        ]
    }

    fn link(root: &Path, name: &str) -> String {
        let path = root.join(name);
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "{name} is not a symlink"
        );
        fs::read_link(path).unwrap().to_str().unwrap().to_owned()
    }

    const LINKS: [(&str, &str, &str); 8] = [
        // (entry, raw target, created target)
        ("pkg/early.txt", "data/real.txt", "data/real.txt"),
        ("pkg/link.txt", "data/real.txt", "data/real.txt"),
        ("pkg/sub/up.txt", "../data/./real.txt", "../data/real.txt"),
        ("pkg/dirlink", "data/", "data"),
        ("pkg/abs.txt", "/pkg/data/real.txt", "data/real.txt"),
        (
            "pkg/sub/abs.txt",
            "//pkg/../pkg/data/real.txt",
            "../data/real.txt",
        ),
        ("pkg/prefixed.txt", "pkg/data/real.txt", "data/real.txt"),
        ("pkg/root", "/", "."),
    ];

    fn check_links(root: &Path) {
        for (entry, _, expected) in LINKS {
            let name = entry.strip_prefix("pkg/").unwrap();
            assert_eq!(link(root, name), expected, "{name}");
        }
        for name in [
            "early.txt",
            "link.txt",
            "sub/up.txt",
            "abs.txt",
            "sub/abs.txt",
            "prefixed.txt",
            "dirlink/real.txt",
            "root/data/real.txt",
        ] {
            assert_eq!(fs::read(root.join(name)).unwrap(), b"real", "{name}");
        }
    }

    #[test]
    fn relative_nested_absolute_and_prefixed_symlinks_for_every_format() {
        let mut tar_entries: Vec<LinkEntry<'_>> = vec![(LINKS[0].0, b'2', LINKS[0].1, b"")];
        tar_entries.push(("pkg/data/real.txt", b'0', "", b"real"));
        tar_entries.extend(
            LINKS[1..]
                .iter()
                .map(|(name, raw, _)| (*name, b'2', *raw, &b""[..])),
        );
        tar_entries.push(("other/ignored", b'2', "../../anything", b""));
        let mut zip_entries: Vec<(&str, Option<&str>, &[u8])> =
            vec![(LINKS[0].0, Some(LINKS[0].1), b"")];
        zip_entries.push(("pkg/data/real.txt", None, b"real"));
        zip_entries.extend(
            LINKS[1..]
                .iter()
                .map(|(name, raw, _)| (*name, Some(*raw), &b""[..])),
        );
        let mut formats = tar_formats(&tar_links(&tar_entries));
        formats.push(("zip", zip_links(&zip_entries)));
        for (kind, bytes) in formats {
            let destination = run(&bytes, kind, "pkg").unwrap_or_else(|e| panic!("{kind}: {e}"));
            check_links(destination.path());
            assert!(!destination.path().join("other").exists(), "{kind}");
        }
    }

    #[test]
    fn hard_links_are_real_links_to_deprefixed_root_paths() {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        let tar = tar_links(&[
            ("pkg/data/real.txt", b'0', "", b"real"),
            ("pkg/hard.txt", b'1', "pkg/data/real.txt", b""),
            ("pkg/sub/abs.txt", b'1', "/pkg/./data/real.txt", b""),
            ("pkg/self.txt", b'0', "", b"self"),
            // GNU tar's repeated-file form: a hard link to itself is dropped.
            ("pkg/self.txt", b'1', "pkg/self.txt", b""),
            // A later hard link replaces an existing regular file.
            ("pkg/replaced.txt", b'0', "", b"old"),
            ("pkg/replaced.txt", b'1', "pkg/data/real.txt", b""),
        ]);
        for (kind, bytes) in tar_formats(&tar) {
            let destination = run(&bytes, kind, "pkg").unwrap_or_else(|e| panic!("{kind}: {e}"));
            let root = destination.path();
            for name in ["hard.txt", "sub/abs.txt", "replaced.txt"] {
                assert_eq!(fs::read(root.join(name)).unwrap(), b"real");
                assert!(
                    !fs::symlink_metadata(root.join(name))
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
                #[cfg(unix)]
                assert_eq!(
                    fs::metadata(root.join(name)).unwrap().ino(),
                    fs::metadata(root.join("data/real.txt")).unwrap().ino()
                );
            }
            #[cfg(unix)]
            assert_eq!(fs::metadata(root.join("data/real.txt")).unwrap().nlink(), 4);
            assert_eq!(fs::read(root.join("self.txt")).unwrap(), b"self");
        }
    }

    #[test]
    fn hard_links_never_reach_outside_missing_or_directory_targets() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"secret").unwrap();
        for (target, message) in [
            ("../secret", "escapes the extraction root"),
            ("pkg/../../secret", "escapes the extraction root"),
            (
                "pkg/missing",
                "File \"missing\" linked from \"hard\" does not exist",
            ),
            (
                "other/file",
                "File \"file\" linked from \"hard\" does not exist",
            ),
            (
                "pkg/later",
                "File \"later\" linked from \"hard\" does not exist",
            ),
            ("pkg/data", "not a regular file"),
            (
                "pkg/data/real.txt/x",
                "File \"x\" linked from \"hard\" does not exist",
            ),
            ("pkg", "extraction root itself"),
        ] {
            let tar = tar_links(&[
                ("other/file", b'0', "", b"outside the prefix"),
                ("pkg/data/real.txt", b'0', "", b"real"),
                ("pkg/hard", b'1', target, b""),
                ("pkg/later", b'0', "", b"later"),
            ]);
            let error = run(&tar, "tar", "pkg").unwrap_err();
            assert!(error.contains(message), "{target}: {error}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(outside.path().join("secret")).unwrap().nlink(),
                1
            );
        }
    }

    #[test]
    fn escaping_tar_symlinks_are_preserved_but_zip_escapes_use_bazel_error() {
        let tar = tar_links(&[
            ("pkg/a/esc", b'2', "../../outside", b""),
            ("pkg/a/sibling", b'2', "../../pkg-sibling/file", b""),
            ("pkg/a/abs", b'2', "/../../etc/passwd", b""),
        ]);
        for (kind, bytes) in tar_formats(&tar) {
            let destination = run(&bytes, kind, "pkg").unwrap();
            assert_eq!(link(destination.path(), "a/esc"), "../../outside");
            assert_eq!(
                link(destination.path(), "a/sibling"),
                "../../pkg-sibling/file"
            );
            // Absolute targets are re-rooted, so `..` cannot leave the root.
            assert_eq!(link(destination.path(), "a/abs"), "../etc/passwd");
        }
        let extract_zip = |entries: &[(&str, Option<&str>, &[u8])], prefix: &str| {
            let destination = tempfile::tempdir().unwrap();
            extract_named(
                &zip_links(entries),
                "zip",
                prefix,
                destination.path(),
                "/downloads/test.zip",
            )
            .map(|()| destination)
        };
        assert_eq!(
            extract_zip(&[("pkg/a/esc", Some("../../outside"), b"")], "pkg").unwrap_err(),
            "Zip entries cannot refer to files outside of their directory: /downloads/test.zip has a symlink a/esc pointing to ../../outside"
        );
        // The check uses the stripped path: inside the archive root, but not
        // inside the stripped repository root.
        assert_eq!(
            extract_zip(
                &[("pkg/a/sibling", Some("../../pkg-sibling/f"), b"")],
                "pkg"
            )
            .unwrap_err(),
            "Zip entries cannot refer to files outside of their directory: /downloads/test.zip has a symlink a/sibling pointing to ../../pkg-sibling/f"
        );
        extract_zip(&[("pkg/a/sibling", Some("../../pkg-sibling/f"), b"")], "").unwrap();
        let fine = extract_zip(
            &[
                ("pkg/a/up", Some("../x"), b""),
                ("pkg/a/cancelled", Some("sub/../../x"), b""),
                ("pkg/a/abs", Some("/../../x"), b""),
            ],
            "pkg",
        )
        .unwrap();
        assert_eq!(link(fine.path(), "a/up"), "../x");
        assert_eq!(link(fine.path(), "a/cancelled"), "../x");
        assert_eq!(link(fine.path(), "a/abs"), "../x");
        // Entries outside the prefix are skipped before the check.
        extract_zip(
            &[
                ("other/esc", Some("../../../x"), b""),
                ("pkg/f", None, b"f"),
            ],
            "pkg",
        )
        .unwrap();
    }

    #[test]
    fn later_entries_cannot_write_through_links() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let attacks: Vec<(&str, Vec<u8>, &str)> = vec![
            (
                "tar",
                tar_links(&[
                    ("evil", b'2', "../../outside", b""),
                    ("evil/payload", b'0', "", b"payload"),
                ]),
                "cannot be deleted",
            ),
            (
                "tar",
                tar_links(&[
                    ("evil/payload", b'0', "", b"payload"),
                    ("evil", b'2', "../../outside", b""),
                ]),
                "cannot be deleted",
            ),
            (
                "tar",
                tar_links(&[
                    ("evil", b'2', "../../outside", b""),
                    ("evil/inner", b'2', "x", b""),
                ]),
                "is below archive symlink \"evil\"",
            ),
            (
                "tar",
                tar_links(&[
                    ("evil", b'2', "../../outside", b""),
                    ("hard", b'1', "evil/secret", b""),
                ]),
                "does not exist",
            ),
            (
                "zip",
                zip_links(&[("evil", Some("x"), b""), ("evil/payload", None, b"payload")]),
                "/evil is not a symlink",
            ),
            (
                "zip",
                zip_links(&[("evil", Some("x"), b""), ("evil/inner", Some("y"), b"")]),
                "is below archive symlink",
            ),
        ];
        fs::write(outside.join("secret"), b"secret").unwrap();
        for (index, (kind, bytes, message)) in attacks.into_iter().enumerate() {
            let destination = temp.path().join(format!("stage-{index}/repo"));
            fs::create_dir_all(&destination).unwrap();
            let error = extract(&bytes, kind, "", &destination).unwrap_err();
            assert!(error.contains(message), "{index}: {error}");
            assert_eq!(
                fs::read_dir(&outside).unwrap().count(),
                1,
                "{index} wrote outside"
            );
        }
    }

    #[test]
    fn links_replace_files_and_empty_directories_last_entry_wins() {
        let tar = tar_links(&[
            ("f", b'0', "", b"file"),
            ("f", b'2', "first", b""),
            ("f", b'2', "second", b""),
            ("d/", b'5', "", b""),
            ("d", b'2', "third", b""),
            ("g", b'2', "fourth", b""),
            ("g", b'0', "", b"written before the deferred link"),
        ]);
        let destination = run(&tar, "tar", "").unwrap();
        assert_eq!(link(destination.path(), "f"), "second");
        assert_eq!(link(destination.path(), "d"), "third");
        assert_eq!(link(destination.path(), "g"), "fourth");
        let zip = zip_links(&[("f", None, b"file"), ("f2", Some("x"), b"")]);
        let destination = run(&zip, "zip", "").unwrap();
        assert_eq!(link(destination.path(), "f2"), "x");
    }

    #[test]
    fn empty_oversized_and_non_utf8_link_targets_are_rejected() {
        for (target, prefix, message) in [
            (".", "", "empty normalized target"),
            ("a/..", "", "empty normalized target"),
            ("pkg", "pkg", "empty normalized target"),
        ] {
            let tar = tar_links(&[("pkg/l", b'2', target, b"")]);
            let error = run(&tar, "tar", prefix).unwrap_err();
            assert!(error.contains(message), "{target}: {error}");
            let zip = zip_links(&[("pkg/l", Some(target), b"")]);
            let error = run(&zip, "zip", prefix).unwrap_err();
            assert!(error.contains(message), "{target}: {error}");
        }
        let long = "x".repeat(256);
        let error = run(&zip_links(&[("l", Some(&long), b"")]), "zip", "").unwrap_err();
        assert!(error.contains("shorter than 256 bytes"), "{error}");
        run(&zip_links(&[("l", Some(&long[1..]), b"")]), "zip", "").unwrap();
        let mut tar = tar_links(&[("l", b'2', "x", b"")]);
        tar[157] = 0xff;
        let mut header = tar::Header::new_ustar();
        header.as_mut_bytes().copy_from_slice(&tar[..512]);
        header.set_cksum();
        tar[..512].copy_from_slice(header.as_bytes());
        assert!(run(&tar, "tar", "").unwrap_err().contains("not UTF-8"));
        // A link entry must carry a link name; other entries must not.
        assert!(
            run(&tar_links(&[("l", b'2', "", b"")]), "tar", "")
                .unwrap_err()
                .contains("empty link name")
        );
        assert!(
            run(&tar_links(&[("f", b'0', "x", b"")]), "tar", "")
                .unwrap_err()
                .contains("tar link names are unsupported")
        );
    }

    /// Rewrite one same-length entry name in both zip headers, producing two
    /// entries with one name (ZipWriter refuses duplicates).
    fn rename_zip(mut bytes: Vec<u8>, from: &str, to: &str) -> Vec<u8> {
        assert_eq!(from.len(), to.len());
        let positions: Vec<_> = bytes
            .windows(from.len())
            .enumerate()
            .filter(|(_, window)| *window == from.as_bytes())
            .map(|(index, _)| index)
            .collect();
        assert_eq!(positions.len(), 2, "local and central header names");
        for index in positions {
            bytes[index..index + to.len()].copy_from_slice(to.as_bytes());
        }
        bytes
    }

    #[test]
    fn zip_links_never_replace_existing_paths_but_later_entries_win() {
        // ZipDecompressor.java:118-120: ensureSymbolicLink without a delete; an
        // existing regular file or directory is `<path> is not a symlink`.
        for link_first in [false, true] {
            let mut entries = vec![
                ("dup.txt", Some("real.txt"), &b""[..]),
                ("./dup.txt", None, &b"file"[..]),
            ];
            if link_first {
                entries.reverse();
            }
            let destination = tempfile::tempdir().unwrap();
            let error = extract(&zip_links(&entries), "zip", "", destination.path()).unwrap_err();
            assert_eq!(
                error,
                format!(
                    "{} is not a symlink",
                    destination.path().join("dup.txt").display()
                )
            );
        }
        // Records with one raw name collapse to the later record, as in Bazel's
        // ZipReader: a later link replaces an earlier file record and vice versa.
        for (first, second, is_link) in [
            (
                ("one.a", None, &b"file"[..]),
                ("one.b", Some("real.txt"), &b""[..]),
                true,
            ),
            (
                ("one.a", Some("real.txt"), &b""[..]),
                ("one.b", None, &b"file"[..]),
                false,
            ),
        ] {
            let bytes = rename_zip(zip_links(&[first, second]), "one.b", "one.a");
            let destination = run(&bytes, "zip", "").unwrap();
            let metadata = fs::symlink_metadata(destination.path().join("one.a")).unwrap();
            assert_eq!(metadata.file_type().is_symlink(), is_link);
        }
        // Two zip links for one path: the later entry wins.
        let bytes = rename_zip(
            zip_links(&[
                ("two.a", Some("first"), b""),
                ("two.b", Some("second"), b""),
            ]),
            "two.b",
            "two.a",
        );
        let destination = run(&bytes, "zip", "").unwrap();
        assert_eq!(link(destination.path(), "two.a"), "second");
        // Two regular zip entries for one path: the later entry wins.
        let bytes = rename_zip(
            zip_links(&[
                ("same.a", None, b"first longer"),
                ("same.b", None, b"second"),
            ]),
            "same.b",
            "same.a",
        );
        let destination = run(&bytes, "zip", "").unwrap();
        assert_eq!(
            fs::read(destination.path().join("same.a")).unwrap(),
            b"second"
        );
        // Tar keeps delete-then-create for a link over a file or empty directory.
        let tar = tar_links(&[
            ("d.txt", b'0', "", b"file"),
            ("d.txt", b'2', "real.txt", b""),
        ]);
        assert_eq!(
            link(run(&tar, "tar", "").unwrap().path(), "d.txt"),
            "real.txt"
        );
    }

    #[cfg(unix)]
    #[test]
    fn later_tar_file_rewrites_a_hard_linked_path_in_place_like_bazel() {
        use std::os::unix::fs::MetadataExt;
        // FileOutputStream truncates the existing inode, so both names change.
        let tar = tar_links(&[
            ("a.txt", b'0', "", b"first"),
            ("b.txt", b'1', "a.txt", b""),
            ("b.txt", b'0', "", b"second"),
        ]);
        let destination = run(&tar, "tar", "").unwrap();
        let root = destination.path();
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"second");
        assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"second");
        assert_eq!(
            fs::metadata(root.join("a.txt")).unwrap().ino(),
            fs::metadata(root.join("b.txt")).unwrap().ino()
        );
        // A regular entry never replaces a directory.
        let tar = tar_links(&[("d/x", b'0', "", b"x"), ("d", b'0', "", b"file")]);
        assert!(
            run(&tar, "tar", "")
                .unwrap_err()
                .contains("conflicts with an existing directory")
        );
    }

    fn xz(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut writer =
            lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::default()).unwrap();
        writer.write_all(bytes).unwrap();
        writer.finish().unwrap()
    }

    fn bz2(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut writer = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        writer.write_all(bytes).unwrap();
        writer.finish().unwrap()
    }

    fn zstd(bytes: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(bytes, ruzstd::encoding::CompressionLevel::Fastest)
    }

    #[test]
    fn xz_zstd_and_bzip2_tar_formats_stream_including_concatenated_streams() {
        let tar = tar_fixture(&[
            ("top/", b'5', 0o755, b""),
            ("top/bin/tool", b'0', 0o755, b"#!/bin/sh\n"),
            ("top/data", b'0', 0o644, &[7u8; 100_000]),
        ]);
        // Split the tar so concatenated streams/frames/members must be joined.
        let (left, right) = tar.split_at(1000);
        let mut skippable = vec![0x50, 0x2a, 0x4d, 0x18, 3, 0, 0, 0, 1, 2, 3];
        skippable.extend(zstd(right));
        for (kind, bytes) in [
            ("tar.xz", xz(&tar)),
            ("txz", [xz(left), xz(right)].concat()),
            ("tar.zst", zstd(&tar)),
            ("tzst", [zstd(left), skippable.clone()].concat()),
            ("tar.bz2", bz2(&tar)),
            ("tbz", [bz2(left), bz2(right)].concat()),
        ] {
            let destination = run(&bytes, kind, "top").unwrap_or_else(|e| panic!("{kind}: {e}"));
            assert_eq!(
                fs::read(destination.path().join("data")).unwrap(),
                vec![7u8; 100_000],
                "{kind}"
            );
            assert_eq!(
                fs::read(destination.path().join("bin/tool")).unwrap(),
                b"#!/bin/sh\n"
            );
        }
    }

    #[test]
    fn extract_file_streams_from_disk_and_reports_prefix_suggestions() {
        let tar = tar_fixture(&[
            ("a/one", b'0', 0o644, b"1"),
            ("b/", b'5', 0o755, b""),
            ("b/c/two", b'0', 0o644, b"2"),
            ("single", b'0', 0o644, b"3"),
        ]);
        let zip = zip_fixture(&[("a/one", 0o644, b"1"), ("b/c/two", 0o644, b"2")]);
        let source = tempfile::tempdir().unwrap();
        for (kind, bytes) in [("tar.gz", gzip(&tar)), ("zip", zip)] {
            let path = source.path().join(format!("archive.{kind}"));
            fs::write(&path, bytes).unwrap();
            let destination = tempfile::tempdir().unwrap();
            extract_file(&path, kind, "b/c", destination.path(), "archive").unwrap();
            assert_eq!(fs::read(destination.path().join("two")).unwrap(), b"2");
            let destination = tempfile::tempdir().unwrap();
            match extract_file(&path, kind, "missing", destination.path(), "archive") {
                Err(Failure::PrefixNotFound(available)) => assert_eq!(
                    available.into_iter().collect::<Vec<_>>(),
                    ["a", "b"],
                    "{kind}"
                ),
                other => panic!("{kind}: {other:?}"),
            }
        }
    }

    #[test]
    fn gnu_long_names_links_and_local_pax_paths_name_the_next_entry() {
        let long = format!("top/{}/file.txt", "d".repeat(120));
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        builder
            .append_data(&mut header, &long, &b"long"[..])
            .unwrap();
        let mut link = tar::Header::new_gnu();
        link.set_size(0);
        link.set_mode(0o777);
        link.set_entry_type(tar::EntryType::Symlink);
        builder
            .append_link(
                &mut link,
                "top/link",
                format!("{}/file.txt", "d".repeat(120)),
            )
            .unwrap();
        let gnu = builder.into_inner().unwrap();
        assert!(gnu.windows(13).any(|w| w == b"././@LongLink"));
        let destination = run(&gnu, "tar", "top").unwrap();
        assert_eq!(
            fs::read(destination.path().join(&long["top/".len()..])).unwrap(),
            b"long"
        );
        assert_eq!(fs::read(destination.path().join("link")).unwrap(), b"long");
        let pax = [
            pax_record("path", format!("top/{}", "p".repeat(150)).as_bytes()),
            pax_record("mtime", b"1700000000.5"),
            pax_record("SCHILY.xattr.user.x", b"v"),
        ]
        .concat();
        let bytes = tar_fixture(&[
            ("PaxHeaders/x", b'x', 0o644, &pax),
            ("truncated-name", b'0', 0o644, b"pax"),
        ]);
        let destination = run(&bytes, "tar", "top").unwrap();
        assert_eq!(
            fs::read(destination.path().join("p".repeat(150))).unwrap(),
            b"pax"
        );
        for (record, message) in [
            (pax_record("GNU.sparse.map", b"0,1"), "unsupported PAX key"),
            (pax_record("size", b"3"), "PAX size records"),
        ] {
            let bytes = tar_fixture(&[
                ("PaxHeaders/x", b'x', 0o644, &record),
                ("f", b'0', 0o644, b"pax"),
            ]);
            assert!(run(&bytes, "tar", "").unwrap_err().contains(message));
        }
        let dangling = tar_fixture(&[("PaxHeaders/x", b'x', 0o644, &pax_record("path", b"a"))]);
        assert!(
            run(&dangling, "tar", "")
                .unwrap_err()
                .contains("no following entry")
        );
        // A long name must still be a safe path.
        let unsafe_name = tar_fixture(&[
            (
                "PaxHeaders/x",
                b'x',
                0o644,
                &pax_record("path", b"../escape"),
            ),
            ("f", b'0', 0o644, b"x"),
        ]);
        assert!(
            run(&unsafe_name, "tar", "")
                .unwrap_err()
                .contains("unsafe archive path")
        );
    }

    #[test]
    fn contiguous_files_extract_as_regular_files_and_huge_extension_records_fail() {
        let bytes = tar_fixture(&[("top/c.bin", b'7', 0o755, b"contiguous")]);
        let destination = run(&bytes, "tar", "top").unwrap();
        assert_eq!(
            fs::read(destination.path().join("c.bin")).unwrap(),
            b"contiguous"
        );
        let huge = vec![b'a'; 1024 * 1024 + 1];
        for kind in *b"LKx" {
            let bytes = tar_fixture(&[
                ("././@LongLink", kind, 0o644, &huge),
                ("f", b'0', 0o644, b"x"),
            ]);
            assert!(
                run(&bytes, "tar", "")
                    .unwrap_err()
                    .contains("exceeds the native 1 MiB limit"),
                "type {kind}"
            );
        }
    }
}
