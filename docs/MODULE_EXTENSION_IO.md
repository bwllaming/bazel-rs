# Bounded module-extension download and read

This is the **candidate native implementation**, not final acceptance or a released
feature. It executes real synchronous HTTP(S) downloads and reads their owned bytes
inside [ordinary module callbacks](MODULE_EXTENSIONS.md). It does not add
`repository_ctx.download`, module `path`, `delete`, `execute`, `file`, facts or
metadata APIs. The [repository-rule profile](REPOSITORY_RULES.md) is unchanged.
No upstream fallback, fabricated download result or SDK map is substituted.

## Signatures and supported arguments

```starlark
module_ctx.download(url, output="", sha256="", executable=False,
                    allow_fail=False, canonical_id="", auth={}, headers={},
                    *, integrity="", block=True)
module_ctx.read(path, *, watch="auto")
```

Arguments before `*` accept positional or keyword binding. Omitted `auth` and
`headers` behave as empty dictionaries; explicit `None` is not supported.

| Argument | Native profile |
|---|---|
| `url` | Required string, or ordered list/tuple of strings; every entry is validated before I/O or reuse. |
| `output`, `path` | Strings naming owned working paths only; not Label or path objects. |
| `sha256`, `integrity` | Strings, default empty; at most one may be nonempty. See checksum grammar below. |
| `executable` | Boolean; only `False`. No executable output mode. |
| `allow_fail` | Boolean, default `False`; only completion failures may return a failure struct. |
| `canonical_id` | String; only empty. No canonical-ID cache behavior. |
| `auth`, `headers` | Dictionaries; only empty. No authentication or custom headers. |
| `block` | Keyword-only Boolean; only `True`. No asynchronous handle or API. |
| `watch` | Keyword-only string; `"auto"` or `"no"`. `"yes"` on owned outputs is unsupported; other values are validation errors. |

Wrong types, unknown arguments and unsupported controls fail explicitly, even with
`allow_fail=True`; they are not disguised as failed network attempts.

### Owned paths and byte reads

Nonempty paths consist of slash-separated ASCII components using only letters,
digits, `.`, `_` and `-`. Empty components, `.`/`..`, absolute paths, backslashes,
non-ASCII names, symlinks, hard-linked files and special files are not admitted.
Limits are listed below. Paths never address workspace, source or generated
repository trees.

`output=""` names the working root itself, **not** a URL-derived basename. The
root starts absent: an empty-output download can make it a regular file, and
`read("")` then reads that file. A named child instead creates parent directories.
If the root is already a file, a child download fails during parent creation;
if it is a directory, downloading to `""` cannot replace it with a file. These
filesystem completion failures may be tolerated; directories are never readable
as file contents.

`read` accepts only files owned by this execution, including old bytes or partial
outputs retained after a tolerated failed download. Missing/unowned paths and
directories fail. Each byte maps directly to U+0000 through U+00FF (ISO-8859-1 /
Latin-1), including NUL and all high bytes. This is **not UTF-8 decoding**, and
UTF-8 re-encoding of the returned string is not a byte-preserving inverse.
Such Latin-1 strings may be passed to repository declarations: repository string
attributes and `ctx.file` contents accept U+0000..U+00FF and write one byte per
character, matching Bazel. Source literals, names, labels and tag attributes stay
ASCII. In extension and repository evaluation `json.decode` turns each `\u` escape
run into its UTF-8 bytes (malformed surrogates become U+FFFD bytes), as Bazel does.
Known divergence and promotion blocker: string methods that Bazel implements with
ASCII-only rules (`upper`, `lower`, `capitalize`, `title`, `strip`, `split`,
`isspace`, `repr`) follow starlark-rust Unicode rules on high characters and may
silently produce different bytes. `print`/stderr of high characters is UTF-8 here
but raw bytes in Bazel; validators compare logical values, not stderr bytes.
`watch="auto"` does not watch these owned files; `"no"` explicitly disables watching.
Neither establishes external-file watching or persistent watch-based reuse.

## URLs, checksums and results

Only absolute HTTP(S) URLs with a host are admitted. URL text must be ASCII with
valid percent escapes and no whitespace/control characters or forbidden URI
characters. Credentials/userinfo and fragments are unsupported; query strings
are retained. `file://` and other schemes are unsupported here.

Initial plain-HTTP entries are removed when **both raw checksum argument strings
are empty**. This test uses nonempty argument text, not successful checksum parsing:
a malformed nonempty supported checksum can retain HTTP and fail later. It is not
a general HTTP bypass option. An empty URL sequence (including one emptied by
filtering) fails validation when `allow_fail=False`, before destination reuse.
With `True`, matching destination reuse is still possible; otherwise it yields a
completion failure without a request.

- `sha256`: exactly 64 hexadecimal characters, either case.
- `integrity`: one lowercase algorithm name followed by `-` and standard Base64:
  `sha1`, `sha256`, `sha384`, `sha512` or `blake3`, with digest lengths of 20, 32,
  48, 64 and 32 bytes respectively. Missing padding and nonzero unused trailing
  bits are accepted; whitespace, extra padding, URL-safe Base64 and multiple SRI
  tokens are not. Returned SRI uses canonical padded Base64.
- Unknown/case-mismatched algorithm prefixes fail as unsupported before I/O.
  Mutual exclusion and checksum-text limits are also immediate errors. Malformed
  hex or Base64/length under a supported algorithm instead records a **delayed
  validation error**: transfer proceeds without a valid expected digest. A
  completed transfer then raises that error, even with `allow_fail=True`; a
  tolerated completion failure returns first. Do not use malformed checksums as
  an integrity guarantee.

Successful results are immutable structs:

| Selected digest | Exact fields |
|---|---|
| None supplied, `sha256`, or SHA-256 SRI | `success=True`, `integrity`, lowercase hex `sha256`, integer `size_bytes` |
| SHA-1, SHA-384, SHA-512 or BLAKE3 SRI | `success=True`, `integrity`, integer `size_bytes`; **no `sha256` field** |
| Tolerated completion failure | Only `success=False` |

No expected checksum selects SHA-256 for successful result reporting, not for
prior integrity verification. Digests and `size_bytes` describe the actual file
bytes: decoded bytes for transparent gzip, compressed bytes when a suffix exception
suppresses decoding. They are not header lengths or character counts.

## Transfer and failure ordering

After argument admission, a valid expected digest matching an existing owned
destination reuses that file without a request. This happens **before** the
download-disable check and is distinct from a persistent repository checksum cache.

The dedicated transport verifies TLS certificates and hostnames, inherits no
ambient proxy or authentication credentials, and sends no automatic referer.
There is no public private-CA injection or TLS-disable option in this profile.
It does not restrict destinations to a host allowlist and is not an SSRF sandbox.

- Mirrors run in order, stopping at the first completed transfer. HTTP 200 and
  206 are accepted; a 200 HTML page is still a successful byte download, not a
  reason to try a second mirror. Application parsing remains the caller's job.
- Redirects 301/302/303/307 are followed; 308 is not. Only a validated 301 updates
  the origin of a connector retry. Temporary redirects do not; the initial URL
  remains separately available for gzip suffix decisions. HTTPS-to-HTTP downgrade
  is a hard safety-policy error.
- Each mirror has at most two connector attempts for connect/timeout failures or
  statuses 403/408/429 and 5xx except 501/505. This is not Bazel's general retry
  policy. Body truncation is not generically retried; range resumption is unsupported.
- `Content-Encoding` values `identity`, `gzip` and `x-gzip` are supported. Gzip is
  decoded unless the **original or final URL path** ends in `.gz` or `.tgz`, ASCII
  case-insensitively. Query text is not a path suffix. Other encodings are explicit
  unsupported-profile errors. A malformed first gzip header fails before opening
  the destination; body/trailer/later-member failures can occur after writes.

With a valid expected checksum, up to **32 KiB (32768 bytes)** are read and hashed
before opening the destination. An earlier EOF permits checksum rejection while
preserving the old file. At exactly 32768 bytes, opening/truncation can precede the
EOF checksum failure. Subsequent writes go directly to the destination: checksum
mismatch or late stream failure can leave actual partial or complete replacement
bytes. Existing permissions are preserved, not reset by chmod. Without a valid
expected digest there is no checksum precheck. There is **no atomic rollback** or
guarantee that a failed download leaves its destination unchanged.

`allow_fail=True` catches only typed completion failures, such as exhausted mirror
I/O/status errors, checksum mismatch, ordinary filesystem failure, or a disabled
download without a matching destination. Validation, unsupported features,
safety-policy violations, resource limits, lifetime violations, interruption and
internal failures remain hard errors. Final result/stat/hash and observation
failures are outside this catch. `read` has no `allow_fail` option.

## Download-disable policy

The user-approved Bazel-default policy permits downloads:
`--repository_disable_download` defaults to **false**. Setting it true gates module
downloads after matching owned destination reuse. It also gates archive, overlay
and registry-patch acquisition after verified tree/blob reuse, **including
`file://` acquisition** in that separate materializer profile. A verified complete
tree remains usable without its download blobs; corrupt/missing cache content does
not authorize acquisition when disabled.

Registry metadata/configuration reads and Git subprocess fetching are **not**
blocked. Local override patch application is not registry-patch acquisition.
This flag is neither offline mode nor an all-network ban; cache reuse does not
prove that an invocation makes no network requests.

`query`, `build` and `mod graph` parse the flag; `mod graph` still does not execute
ordinary extensions. The legacy alias is `--experimental_repository_disable_download`.
Both positive names accept `=true/1/yes/t/y` or `=false/0/no/f/n`, case-insensitively;
without `=` they mean true. `--norepository_disable_download` and
`--noexperimental_repository_disable_download` mean false and accept no value.
Split values are not consumed; the last occurrence wins.

Existing public library constructors/materializer wrappers remain default-enabled.
`RepositorySet::with_repository_downloads_enabled(bool)` and
`materializer::materialize_with_download_policy` carry the explicit policy.
Policy changes use fresh runtime demand slots; the policy bit itself is excluded
from persistent tree/blob keys, manifests and repository/extension provenance.
Observed I/O outcomes can still change provenance.

## Execution limits and lifetime

These are native bounds, not claims about Bazel's default limits. Cumulative
limits below are per callback execution and shared across its I/O operations.

| Resource | Bound |
|---|---|
| Mirrors | 8 per download |
| Connector attempts | 2 per mirror |
| Followed redirects | 8 per attempt |
| HTTP requests | 64 total, including retries/redirects |
| Operations | 32 combined download/read calls |
| Resulting file bytes | 8 MiB per file |
| Encoded / decoded body bytes | Separate 32 MiB cumulative counters |
| Live scratch file bytes | 16 MiB |
| Conservative cumulative I/O work/buffer charge | 128 MiB |
| Path | 1024 bytes, 32 components, 128 bytes per component |
| URL / checksum text | 8192 / 256 bytes per string |
| Execution-I/O deadline | 120 seconds, cooperative; connection timeout at most 20 seconds |

Reads are bounded before requesting decoded output, including the checksum
precheck. At an exact byte cap, an EOF probe may consume **one excess byte**; it is
counted and rejected, never hashed, written or delivered as successful content.
Exceeding the cumulative decoded cap prevents further probes. Counters measure
actual requested/returned stream bytes, not every private inflater dictionary
operation or look-ahead. Encoded body accounting excludes headers/wire framing
and transport-internal buffers. The work charge includes repeated reads/hashes,
writes, precheck buffers, Latin-1 expansion and URL copies; it is **not a hard
process-memory cap**.

The deadline is checked around transport/read operations and supplies remaining
request timeouts. Blocking DNS cannot be forcibly canceled: there is **no hard DNS
wall-clock guarantee** or general process-cancellation feature.

Scratch storage is a lazily allocated host-owned RAII container with an initially
absent working child. On Unix, the outer container requests0700 **at creation**,
without a create-then-chmod window or reliance on a caller's ordinary0022/0077
umask to remove group/other access. Working-child creation and downloaded-file
modes are unchanged: the private ancestor protects them; fresh0644 and retained
seed0640 contracts are not replaced by this directory policy. This is not a
privacy guarantee against privileged/same-UID processes or filesystems that do
not enforce these modes. Non-Unix behavior is unchanged; no Windows ACL or
cross-platform privacy guarantee is claimed. Starlark contexts retain only a
nonreused inert execution identity, not paths, clients or handles. An active matching receiver is required;
foreign, frozen and expired receivers cannot borrow I/O authority. Client shutdown
and explicit cleanup precede callback freezing; Drop covers errors/unwind with
best-effort cleanup. Process kill can prevent cleanup. Ownership checks are not a
sandbox against hostile same-UID filesystem mutation.

## Provenance and reuse

Nonempty completed-operation observations add optional
`extension_io = {"version": 1, "observations": [...]}` to bounded ordinary-extension
provenance. With zero I/O operations, the **entire field is omitted**, preserving
the previous serialization bytes, field order and persistent keys.

Observations are ordered: downloads record relative output, reuse, public result
and resulting missing/directory/file state; reads record relative path, watch
choice and file state. File state includes generation, byte SHA-256 and size.
Overwrites and tolerated partial-failure states therefore contribute, even if
the caller ignores the response or emits constant generated content. This is a
conservative overapproximation of dependencies, not minimal dataflow tracking.

The new observations contain no random absolute paths, execution IDs, timing,
raw URLs or response bodies. Existing source provenance still contains original
source bytes (including any embedded URLs); this is **not blanket secret redaction**.
The whole callback and all declaration/import validation must succeed before
generated-repository publication.

A fresh runtime reevaluates callbacks and refetches mutable metadata before
verified publication reuse. Within the same runtime, existing `DemandSlot`
memoization remains. There is no URL-only persistent metadata cache, facts reuse,
watch reuse or lockfile implementation. Stable generated/action cache keys are
not evidence of download-free invocations. SDK extraction/execution, configured
toolchains and full application/Bazel parity remain unestablished.

## Evidence boundaries

The 1,284-Rust and 851-Python checkpoints are historical, not final combined
acceptance. The first native collector capture completed zero cases; its exit-zero
build did not supply retained artifact/mode/cache proof after the checker aborted.
Subsequent collector hardening requires independent review and fresh bound captures.
The 23 additional evidence lanes are not implied by the default 33-case inventory
or aggregate test counts. Missing native APIs and inaccessible genuine path values
remain explicit exclusions, not passing rejection cases.

The [runtime handoff](../artifacts/n3-runtime-handoff.md) and
[transport remediation receipt](../artifacts/n3-transport-ready.md) retain scoped
native verification, historical failures and cleanup evidence. Their scoped test
counts are not final whole-candidate totals or release acceptance. Native CLI
evidence uses checksummed HTTP through real generated repositories and consumers.
Separate native tests inject an internal private CA into the real TLS client and
exercise checksumless HTTPS/read; genuine Bazel reference captures use a private
JVM truststore. Neither substitutes for **native CLI checksumless HTTPS acceptance**,
which remains unestablished. No clean-LSP claim is made.

Reference captures under `/tmp/opencode/bazel-module-download-contract/` retain
`REPORT.md`, `continuation/REPORT.md`, `supplemental/REPORT.md` and
`supplemental-continuation/REPORT.md` and `fresh-boundaries-v1/REPORT.md`, against Bazel 9.3 source
`e89755babf3e941bc1e9d8120b1cd48854ab9dfa`. Original failed overwrite/truncation
expectations remain failed; later observations do not relabel them. The earlier
full Rust gate's symlink failure remains unexplained. The failed intermediate log
is `extension-io-runtime-rust-after-fixes.log`; the later 1,284-test pass is retained
in [extension-io-final-check.log](../artifacts/extension-io-final-check.log).
The latter is a separate observation, not a root-cause fix or final acceptance.

Fresh reference observations establish absent/partial-file checksum boundaries,
terminal truncation, and default-enabled seeded0640 reuse under their recorded
profile. Fresh0644 was observed with child umask0022 on the owned filesystem; it
is not an unconditional filesystem-mode guarantee. Reference ctx.path/ctx.execute
instrumentation does not expose those native APIs, and native tests/captures must
establish their own corresponding results.

Current implementation authority is [argument binding](../src/extension_ctx.rs),
[transport and checksums](../src/extension_download.rs),
[owned I/O](../src/extension_io.rs), [provenance](../src/repository.rs) and
[materializer policy](../src/materializer.rs). Historical plans/receipts do not
widen these supported surfaces.
