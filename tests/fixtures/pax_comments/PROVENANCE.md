# Genuine PAX global-comment archive fixture

This fixture pins **bazel_features 1.42.1** for archive compatibility testing.
It does not establish the version selected by any separate application build.

`bazel_features-1.42.1.tar.gz` is the unchanged upstream release source archive:

- URL: https://github.com/bazel-contrib/bazel_features/releases/download/v1.42.1/bazel_features-v1.42.1.tar.gz
- Length: 18,078 bytes
- SHA-256: `8189bac9a6bf9cc155a854c4cbebfebf58b9ca7a2d0a67645f7d0c1f83c523ac`
- SRI: `sha256-gYm6yaa/nMFVqFTEy+v+v1i5ynotCmdkX30MH4PFI6w=`
- Strip prefix: `bazel_features-1.42.1`
- Release tag commit: `0b1ba9d4606e0bf4cc9a1694f7cb2e114792cc22`
- License: Apache License 2.0, retained as the original archive member `LICENSE`
  and a byte-identical standalone `LICENSE` next to this provenance file.
  Its SHA-256 is `c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4`.

`source.json` is byte-identical to the captured descriptor at
`tests/fixtures/builtin_modules/registry/modules/bazel_features/1.42.1/source.json`
and the BCR descriptor at commit
`1e49ce0ad6fbeea2ab0cc73fda6ee0a1ea18330d`:
https://github.com/bazelbuild/bazel-central-registry/blob/1e49ce0ad6fbeea2ab0cc73fda6ee0a1ea18330d/modules/bazel_features/1.42.1/source.json
Descriptor SHA-256: `fcd4396b2df85f64f2b3bb436ad870793ecf39180f1d796f913cc9276d355309`.
Its `docs_url` identifies a separate archive, not this source fixture. The BCR
patch is recorded for provenance but is not applied in archive-level checks.

## Complete archive contract

The gzip expands to 92,160 bytes. Its 49 physical tar headers comprise one
global PAX (`g`) header, seven directories, and 41 regular files containing
48,904 bytes. All header checksums were verified. There are no links, local PAX,
GNU long-name headers, sparse entries, or other special entries. The first end
block is at offset 87,040; the remaining 5,120 bytes are zero.

The first header is named `pax_global_header`, mode 0666, with this exact
52-byte payload (the notation `\n` denotes one LF byte):

```text
52 comment=0b1ba9d4606e0bf4cc9a1694f7cb2e114792cc22\n
```

It is metadata, not an extracted file or a strip-prefix match.

`expected_manifest.json` includes **every** regular file, including hidden,
test, licensing, and upstream BUILD/MODULE files. Keys are relative to the
stripped root. `directories` includes the archive root as the empty string.
File `mode` values are decimal native-normalized permissions: 493 (0755) when
any archive execute bit is set, otherwise 420 (0644). Original file modes are
0664 except `.github/workflows/release_prep.sh` (0775); all directory members
are 0775. Native-normalized modes are not an assertion of Bazel metadata parity:
the upstream extractor applies mode | 0400 instead.

Raw physical-member inspection, Python `tarfile`, and Bazel-pinned Apache
Commons Compress 1.26.1 independently agree on all 41 paths, lengths, and
SHA-256 values. The fixture contains no header stripping, archive repackaging,
member filtering, or license removal. The original Apache license remains
inside the archive; its standalone copy also preserves the original bytes.

CLI QA may use an explicitly separate generated registry descriptor with the
same archive integrity, a loopback download URL, a minimal registry MODULE,
and a root BUILD overlay solely to copy LICENSE. That tests archive extraction
and supported registry overlay behavior, not execution of upstream BUILD or
its dependency/extension language. Whole-original-member extraction is checked
separately against the complete manifest before any overlays.
