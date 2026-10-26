# Remote registry and archive profile

This remains a native, incomplete Bazel 9.3 implementation, not full parity.
`query`/`build` support every `--lockfile_mode` (default `update`, which reads
and writes `MODULE.bazel.lock`; see `docs/REGISTRY_LOCKFILE.md`); so does `mod`
(docs/MOD_COMMAND.md). Without
`--registry`, the registry list is Bazel 9.3's default `https://bcr.bazel.build/`
(`BazelRepositoryModule.java:133`); any explicit `--registry` replaces it.
Example:

```sh
bazel-rs --ignore_all_rc_files --cache-dir=/absolute/cache query \
  --registry=https://bcr.bazel.build --lockfile_mode=off @dependency//:target
```

Registry MODULE files, metadata and source descriptors retain their actual origin.
Only missing MODULE responses (HTTP 404/410) advance to another registry. An
invalid MODULE or other transport failure is an error. Source descriptors and
overlays always come from the selected registry, never a fallback registry.
Graph-only operations do not download source archives. Main-only source queries
continue to avoid evaluating the module graph.

HTTP transport uses verified TLS, bounded redirects without HTTPS downgrade and
Bazel's HttpConnector timeouts (10 s connect, 20 s per read; no whole-transfer
deadline); there are no size limits (N32). Credentials, credential helpers,
environment proxies and automatic content decoding are not enabled. Registry
files are fetched once per invocation; across invocations only files whose
SHA-256 is recorded in MODULE.bazel.lock are reused (from the repository cache),
never under an untrusted name/version key. This deliberately does not implement
authentication, proxy configuration, conditional HTTP caching or all Bazel
downloader policies.

Archive sources support tar, tar.gz/tgz, tar.xz/txz, tar.zst/tzst, tar.bz2/tbz
and stored/deflated ZIP (type inferred with Bazel's suffix search order), with
SHA-1, SHA-256, SHA-384, SHA-512 or BLAKE3 SRI verification. Registry mirrors precede
the primary URL; descriptor mirrors follow it. Checksum failures never publish a
blob or repository. The registry itself supplies the digest: HTTP registry access
does **not** authenticate that digest; use trusted HTTPS or local fixtures.

The explicitly recognized `docs_url` source field is inert documentation
metadata. Like Bazel 9.3's archive deserializer, native materialization ignores
its value regardless of JSON type: it is never validated as a URL, fetched, or
used as a mirror. Other unsupported source fields still fail. The complete
parsed descriptor remains part of native tree identity, so a documentation-only
change can produce a separate tree while reusing the verified archive blob.
This preserves parsed metadata values, not original JSON formatting.

The `docs_url` correction passes nine genuine JSON-value comparisons and two
native rejection checks; HTTP traces show no documentation requests. The
preserved application probe at that increment advanced to an unsupported `XGlobalHeader`
entry in a demanded archive. General PAX support is not implied by accepting
documentation metadata. See `artifacts/archive-docs-url-validation.json` and
`artifacts/mainrepo-archive-docs-url-loading.json`.

Extraction uses a private staging directory. Regular UTF-8 files/directories,
tar/zip symbolic links and tar hard links are accepted (links follow Bazel 9.3,
see `docs/ARCHIVE_SYMLINKS.md`); traversal, absolute entry paths, special entries,
semantic PAX metadata (sparse files, PAX `size`, unknown keys), extension
records over 1 MiB and conflicting files fail explicitly. GNU long names/links
(`L`/`K`) and local PAX `path`/`linkpath` name the next entry like
commons-compress; type `7` (contiguous) entries are regular files.
Global PAX headers containing only inert `comment` records are supported as
described below. This is stricter than Bazel's
extraction semantics. Executable files become 0755, others 0644. Source
archives stream into a temporary file of the content-addressed download cache
while being hashed and are published by atomic rename only after verification
(`downloads/<algorithm>/<hex>`); extraction streams from that blob, and overlays
are copied from their blobs. There are no download, expanded-size, entry-count
or metadata/overlay size caps (N32; Bazel has none).

### Bounded global PAX comments

Raw tar iteration consumes global `g` headers only when every record is a
strictly byte-length-framed `comment`. Parsing uses fixed-capacity buffering and
checked lengths; it never splits values by lines or allocates a payload-sized
string. Values are ignored opaque bytes, including embedded newlines, equals
signs, NULs and non-UTF-8 bytes. This is not a claim of strict POSIX text validation.
Empty headers, repeated comments, empty values and leading-zero lengths are
accepted. There is no PAX record-count cap (N32); records are still consumed
in small, fixed-size reads.

Metadata names must still be safe, nonempty relative paths; mode syntax and link
fields are validated. These are explicit native restrictions on pseudo-header
names, not complete Bazel/POSIX metadata-name compatibility. Headers are never
written as files and cannot satisfy `strip_prefix`. Each global header must have
a following validated regular-file, directory or link entry, even if that entry is
filtered out by the prefix. A dangling global is rejected after trailer checks.

All other global keys—including path, size, link, ownership, time, sparse and
vendor metadata—remain unsupported, as do local PAX and GNU extensions. Adding
semantic PAX fields later requires metadata-aware framing and effective-path
validation, not silently dropping those fields. No archive bytes are rewritten
to bypass these restrictions.

The unchanged `bazel_features` 1.42.1 release fixture has one global comment,
41 files and seven directory members. Its complete extracted manifest is checked
before any registry overlays. Separate real CLI checks use an explicitly declared
BUILD overlay as a test consumer; they are not evidence that upstream BUILD code
is supported. Three positive comparisons and seven native rejection cases are
recorded in `artifacts/pax-comments-validation.json`.

The preserved application probe at the archive increment passed extraction and failed at
`attr.string_list_dict` in genuine `bazel_features` repository code. The fixture
version does not establish the application's selected version. See
`artifacts/mainrepo-pax-comments-loading.json`; no application-build success is
claimed.

The subsequent dictionary-attribute increment advances that same probe to
`repository_rule(local=True)`; see `docs/STRING_LIST_DICT.md` for its typed-value
profile and current evidence.

Verified overlays replace files and are executable. Ordered registry patches
then apply, original registry MODULE bytes replace the extracted MODULE, and
ordered local override patches apply last. Discovery uses a separate MODULE-only
projection of those local patches. The supported text-patch profile is bounded;
see `docs/PATCHES.md`. Unsupported transformations fail rather than being omitted.

Repository storage lives below `--cache-dir/repositories` (by default,
`bazel-rs-out/repositories`). Publishers serialize with filesystem locks and
atomically rename completed trees. Subsequent accesses check the file list,
contents and permissions; corrupt trees rebuild using reverified download blobs.
Metadata remains fresh even on a warm archive cache. The cache and its ancestors
must be trusted against hostile concurrent mutation; local actions run
sandboxed by default (docs/SANDBOX.md), but that sandbox denies only
out-of-sandbox writes — network is allowed by default, reads and metadata
operations stay unrestricted — so it is containment, not a security boundary.
There is no promise of offline operation, crash-durable extracted
file contents, or compatibility with Bazel's on-disk cache format.

Downloads default to enabled. `--repository_disable_download` gates module-context
and archive/overlay/patch acquisition after eligible verified reuse; it also gates
the materializer's file acquisition. Registry metadata/configuration and Git
subprocess fetching remain outside its scope. `--nofetch` remains unsupported;
the download-disable flag is not an offline mode. Full lockfile,
extension/repository-rule, toolchain, configured
analysis and application build/test compatibility remain separate requirements.

## Historical archive-increment validation

`artifacts/remote-validation.json` passes 17 checks: three differential groups
(tar, tar.gz, ZIP query/source build/genrule artifacts) against SHA256-pinned
Bazel 9.3, and fourteen separately labelled native-only checks. The latter cover
warm caches, corruption repair, registry fallback/errors, lazy source fetching,
integrity/path failures and the two reviewer regressions. They are not negative
diagnostic parity claims. HTTP request logs, raw command results, artifact hashes
and HTTP/reference/scratch cleanup receipts are retained in that report.

Since N26 (PatchUtil port) the former native-only `binary-patch-unsupported`
rejection is retired: genuine Bazel 9.3 applies a header-less `GIT binary patch`
registry patch as a no-op, so the plan now has four differential groups
(`differential.binary-patch-text` compares exact query/build bytes, one patch
fetch per engine and the full unpatched tree inventory) and thirteen native-only
checks. That historical report predates this change.

At that increment, the full check passed 328 Rust tests and 143 Python harness tests, strict Clippy,
formatting, release compilation and rust-analyzer error diagnostics. The existing
local/custom/loading/module suites remain passing; their reports record their own
binary hashes rather than implying all were rerun after archive-only fixes.

That increment's source-preserving HTTPS application probe reached `rules_swift@3.1.2` and
failed at unsupported `use_repo_rule`. See `artifacts/mainrepo-remote-loading.json`.
This is progress past missing remote registries, not application build/test
compatibility or evidence of a whole-application speedup.
