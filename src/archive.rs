//! Native extraction into caller-owned, private staging directories.
//!
//! Links, special files, non-UTF-8 names, and semantic tar extensions are
//! deliberately unsupported. Only strictly framed global PAX comments are
//! consumed as inert metadata; local PAX and GNU extensions remain rejected.
//! File permissions are normalized to 0755 when any executable bit is set, or
//! 0644 otherwise; directories use 0755. Ownership and privileged bits are not
//! preserved. The caller must remove staging on failure and exclude concurrent
//! writers: metadata checks are not a defense against a hostile concurrent rename.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Cursor, Read};
use std::path::{Path, PathBuf};

const MAX_EXPANDED: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;
const MAX_PAX_RECORDS: usize = 100_000;

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
        entries: 0,
        expanded: 0,
    };
    match archive_type {
        "tar" => extract_tar(Cursor::new(bytes), &mut state)?,
        "tar.gz" | "tgz" => {
            extract_tar(flate2::read::MultiGzDecoder::new(bytes), &mut state)?;
        }
        "zip" => extract_zip(bytes, &mut state)?,
        other => return Err(format!("unsupported archive type: {other}")),
    }
    if !state.matched {
        return Err(format!(
            "archive strip_prefix {strip_prefix:?} did not match"
        ));
    }
    Ok(())
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
    entries: usize,
    expanded: u64,
}

impl Extraction<'_> {
    fn account(&mut self, size: u64) -> Result<(), String> {
        self.entries += 1;
        if self.entries > MAX_ENTRIES {
            return Err("archive exceeds entry limit (100000)".into());
        }
        self.expanded = self
            .expanded
            .checked_add(size)
            .filter(|total| *total <= MAX_EXPANDED)
            .ok_or("archive exceeds expanded size limit (1 GiB)")?;
        Ok(())
    }

    fn output(&mut self, raw: &[u8], directory: bool) -> Result<Option<PathBuf>, String> {
        let parts = safe_components(raw)?;
        if parts.is_empty() && !directory {
            return Err("archive file has an empty normalized path".into());
        }
        if !parts.starts_with(&self.prefix) {
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
        Ok(Some(output))
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
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| format!("create archive file {path:?} (duplicate or conflict): {e}"))?;
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

// Bound decompression of tar framing, skipped entries, and trailing gzip
// members too, rather than merely counting extracted file payloads.
struct BoundedReader<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let limit = buffer.len().min((self.remaining + 1) as usize);
        let count = self.inner.read(&mut buffer[..limit])?;
        if count as u64 > self.remaining {
            return Err(io::Error::other(
                "archive exceeds expanded size limit (1 GiB)",
            ));
        }
        self.remaining -= count as u64;
        Ok(count)
    }
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
        if *records >= MAX_PAX_RECORDS {
            return Err("archive exceeds PAX record count limit".into());
        }
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
    let mut archive = tar::Archive::new(BoundedReader {
        inner: reader,
        remaining: MAX_EXPANDED,
    });
    // Raw iteration prevents the library from buffering arbitrarily large GNU
    // and PAX metadata records, or silently ignoring their unsafe paths.
    let entries = archive
        .entries()
        .map_err(|e| format!("read tar archive: {e}"))?
        .raw(true);
    let mut pax_records = 0;
    let mut pending_global = false;
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
        if !directory && !kind.is_file() && !global {
            return Err(format!(
                "unsupported tar entry type (links, special files and extension records are rejected): {kind:?}"
            ));
        }
        if entry.link_name_bytes().is_some() {
            return Err("tar link names are unsupported, including on regular entries".into());
        }
        let size = entry.size();
        if directory && size != 0 {
            return Err("tar directory has nonempty file data".into());
        }
        let mode = header
            .mode()
            .map_err(|e| format!("invalid tar mode: {e}"))?;
        state.account(size)?;
        if global {
            // A metadata name is never an output or a strip-prefix match. Keep
            // the native safe-relative-path restriction even for these names.
            if safe_components(&entry.path_bytes())?.is_empty() {
                return Err("global PAX header requires a nonempty safe path".into());
            }
            consume_global_comments(&mut entry, size, &mut pax_records)?;
            pending_global = true;
            continue;
        }
        let output = state.output(&entry.path_bytes(), directory)?;
        if let Some(output) = output {
            if directory {
                state.directory(&output)?;
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
    Ok(())
}

fn zip_entry_info<R: Read + ?Sized>(
    entry: &zip::read::ZipFile<'_, R>,
) -> Result<(bool, u32), String> {
    safe_components(entry.name_raw())?;
    let directory = entry.is_dir();
    let mode = entry.unix_mode().unwrap_or(0o644);
    match mode & 0o170000 {
        0 => (),
        0o040000 if directory => (),
        0o100000 if !directory => (),
        _ => {
            return Err("unsupported zip entry type (links and special files are rejected)".into());
        }
    }
    if directory && entry.size() != 0 {
        return Err("zip directory has nonempty file data".into());
    }
    Ok((directory, mode))
}

fn extract_zip(bytes: &[u8], state: &mut Extraction<'_>) -> Result<(), String> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("read zip archive: {e}"))?;
    if archive.len() > MAX_ENTRIES {
        return Err("archive exceeds entry limit (100000)".into());
    }
    // Preflight every path, type and declared size, even outside strip_prefix.
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|e| format!("read zip entry: {e}"))?;
        zip_entry_info(&entry)?;
        state.account(entry.size())?;
    }
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("read zip entry: {e}"))?;
        let (directory, mode) = zip_entry_info(&entry)?;
        let output = state.output(entry.name_raw(), directory)?;
        if let Some(output) = &output {
            if directory {
                state.directory(output)?;
            } else {
                state.file(output, mode, entry.size(), &mut entry)?;
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
    fn tar_links_special_files_and_extensions_are_explicit_errors() {
        for kind in *b"123467xLKS" {
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
        let mut records = MAX_PAX_RECORDS - 1;
        consume_global_comments(
            SmallReads(Cursor::new(&record)),
            record.len() as u64,
            &mut records,
        )
        .unwrap();
        assert_eq!(records, MAX_PAX_RECORDS);
        assert!(
            consume_global_comments(Cursor::new(b"12 comment=\n"), 12, &mut records)
                .unwrap_err()
                .contains("record count")
        );
        // Empty payloads contain no records; headers have their separate budget.
        consume_global_comments(Cursor::new(b""), 0, &mut records).unwrap();
        let first = b"12 comment=\n".repeat(MAX_PAX_RECORDS / 2);
        let second = b"12 comment=\n".repeat(MAX_PAX_RECORDS / 2 + 1);
        let bytes = tar_fixture(&[
            ("first", b'g', 0o644, &first),
            ("second", b'g', 0o644, &second),
            ("f", b'0', 0o644, b"data"),
        ]);
        assert!(
            run(&bytes, "tar", "")
                .unwrap_err()
                .contains("PAX record count")
        );
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
    fn zip_links_and_special_files_are_rejected_before_filtering() {
        for kind in [0o120000u32, 0o010000, 0o060000] {
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
    fn directory_duplicates_allowed_but_file_conflicts_rejected() {
        let good = tar_fixture(&[
            ("dir/", b'5', 0o755, b""),
            ("dir/", b'5', 0o755, b""),
            ("dir/file", b'0', 0o644, b"ok"),
        ]);
        assert!(run(&good, "tar", "").is_ok());
        for entries in [
            vec![
                ("file", b'0', 0o644, &b"one"[..]),
                ("./file", b'0', 0o644, &b"two"[..]),
            ],
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
        let mut header = tar::Header::new_ustar();
        header.set_path("outside-prefix").unwrap();
        header.set_mode(0o644);
        header.set_size(MAX_EXPANDED + 1);
        header.set_cksum();
        assert!(
            run(header.as_bytes(), "tar", "wanted")
                .unwrap_err()
                .contains("expanded size limit")
        );
        let destination = tempfile::tempdir().unwrap();
        let mut state = Extraction {
            destination: destination.path(),
            prefix: vec![],
            matched: true,
            entries: MAX_ENTRIES,
            expanded: 0,
        };
        assert!(state.account(0).unwrap_err().contains("entry limit"));
        let mut limited = BoundedReader {
            inner: Cursor::new(b"1234"),
            remaining: 3,
        };
        assert!(io::copy(&mut limited, &mut io::sink()).is_err());
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
            entries: 0,
            expanded: 0,
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
}
