# `.ar` and `.deb` archives

`repository_ctx.extract`, `repository_ctx.download_and_extract` (and so
`@bazel_tools` `http_archive`, e.g. rules_distroless' `deb_import` of Debian
packages), `module_ctx.extract`/`download_and_extract` and registry archives
extract `ar` archives like Bazel 9.3's ArFunction
(`bazel/repository/decompressor/ArFunction.java`) over commons-compress
1.26.1's `ArArchiveInputStream` (`src/archive.rs`, `extract_ar_file`). The
genuine capture is `tests/fixtures/ar_archives/reference_receipt.json`
(62 cases; replayed by `tests/ar_archives.rs`).

## Type inference

* `ar` and `deb` are one decompressor (DecompressorValue.java:151-152, 172).
  `extract(type = ...)` selects it by type name, otherwise by the archive's
  suffix in Bazel's search order.
* `download_and_extract` always selects by file name: the download lands in
  `<output>/temp<N>/<name>` where `<name>` is DownloadManager's
  `getDownloadDestination` (java:522-536): the FIRST URL's percent-decoded
  basename (`temp` when empty, `cacheprobe[.TYPE]` without URLs), `.TYPE`
  appended unless it already ends with it, every UTF-16 unit outside
  `[A-Za-z0-9._-]` replaced by `_`. So `bad+name.deb` is `bad_name.deb`,
  `type = "ar"` makes it `bad_name.deb.ar`, and `type = "rar"` fails with
  `Expected a file with a ... suffix (got .../basic.deb.rar)`. The same name
  appears in `Error downloading [...] to PATH` and `Error extracting` texts.

## Members

* Every member becomes a regular file at
  `destination.getRelative(rename_files.get(name, name))`; there are no
  directory, link or special members. `strip_prefix` and `strip_components`
  are ignored (no `Prefix ... was given` error); `rename_files` applies to
  member names (and may create subdirectories). The data members of a `.deb`
  (`debian-binary`, `control.tar.*`, `data.tar.*`) are written as they are:
  nothing recurses into `data.tar.*`.
* Names: the 16-byte field is `String.trim()`med; `//` is the GNU string
  table (replacing any earlier one), a trailing `/` (GNU terminator) is
  dropped, `/<digits>` names the table entry at that offset (up to its LF or
  NUL, without one trailing `/`), `#1/<digits>` is a BSD name stored before
  the data (its length counts in the size field); anything else is the plain
  name. The result is PathFragment-normalized (`./a`, `x//y`, `d/../up`), its
  parents are created like `createDirectoryAndParents`, and a later member
  with the same name truncates and rewrites the file in place.
* Modes: `chmod(mode | 0400)` keeps the low 12 bits, so `100070` is `0470`
  and setuid/setgid/sticky bits are applied (the native repository store then
  refuses such repositories: `special permission bits in direct repository
  are unsupported`, a declared divergence).
* Times: atime and mtime are the member's timestamp (utimensat, also `0`
  and negative values); numbers parse like `Integer.parseInt`/
  `Long.parseLong` (`+5` is 5; uid/gid may be blank, mode and mtime may not).
* The output of `extract` is created only when a member is written (Bazel
  9.3 deletes a repository directory before its implementation runs,
  RepositoryFetchFunction.setupRepoRoot); `download_and_extract` creates its
  output first. A member with an empty name (the GNU symbol table `/`) is
  `getOutputStream()` of the destination itself: `<output> (Is a directory)`
  when it exists, otherwise Bazel writes a regular file there and the next
  member fails with `<output> (File exists)`; the native profile reports
  that error, and refuses explicitly when the symbol table is the last member.

## Errors

Every commons-compress and filesystem IOException is wrapped like
DecompressorValue.decompress: `java.io.IOException: Error extracting ARCHIVE
to PARENT: MESSAGE`, with commons-compress 1.26.1's texts:
`Failed to read header. Occurred at byte: N`, `Invalid header XXXXXXXX` (the
8 bytes; control bytes raw, bytes above 0x7F as `?`), `Truncated ar archive`
(also for trailing bytes that are not a full header), `Failed to read entry
trailer. Occurred at byte: N`, `Invalid entry trailer. not read the content?
Occurred at byte: N`, `Unable to parse int|long from string value: TEXT`,
`broken archive, entry with negative size`, `Cannot process GNU long file
name as no // record was found`, `Failed to read entry: OFFSET`, `Failed to
read complete // record: expected=N read=M`, `null` (a BSD name cut short:
`new EOFException()`), and Bazel's own `PATH (File exists)`, `PATH (Is a
directory)`, `PATH (Permission denied)` and `Invalid file path` (NUL in a
name). Data cut short by the end of the archive is NOT an error: the member
is extracted short, like commons-compress' `read`.

## Native restrictions (declared)

* A name that is absolute or leaves the extraction destination (`../x`) is
  an explicit error; Bazel writes beside the destination.
* A member name with bytes above 0x7F is an explicit error; Bazel 9.3
  crashes on it (exit 37, IllegalStateException in
  StringUnsafe.getInternalStringBytes for the U+FFFD commons-compress decodes).
* Extraction streams into a private staging directory of the repository and
  is merged into the output afterwards (errors name the output, as Bazel's
  do); a pre-existing file of the output is replaced rather than truncated in
  place.
