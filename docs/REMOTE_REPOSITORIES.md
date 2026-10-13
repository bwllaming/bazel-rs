# Remote registry and archive profile

This remains a native, incomplete Bazel 9.3 implementation, not full parity.
Remote registry reads require an explicit `--lockfile_mode=off`; no lockfile is
read or written. No default network registry is injected. Example:

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

HTTP transport uses verified TLS, bounded redirects without HTTPS downgrade,
connection/request deadlines and size limits. Credentials, credential helpers,
environment proxies and automatic content decoding are not enabled. Metadata is
fresh per resolution rather than cached under an untrusted name/version key.
This deliberately does not implement authentication, proxy configuration,
conditional HTTP caching, lockfile pins or all Bazel downloader policies.

Archive sources support tar, gzip-compressed tar and stored/deflated ZIP, with
SHA-1, SHA-256, SHA-384, SHA-512 or BLAKE3 SRI verification. Registry mirrors precede
the primary URL; descriptor mirrors follow it. Checksum failures never publish a
blob or repository. The registry itself supplies the digest: HTTP registry access
does **not** authenticate that digest; use trusted HTTPS or local fixtures.

Extraction uses a private staging directory. Only regular UTF-8 files/directories
are accepted; traversal, absolute paths, links, special entries, GNU/PAX extension
records and conflicting files fail explicitly. This is stricter than Bazel's
extraction semantics. Executable files become 0755, others 0644. Downloads are
limited to 256 MiB, expanded data to 1 GiB and entries to 100000; metadata/overlay
files are limited to 16 MiB. These are explicit native-profile limits, not parity.

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
must be trusted against hostile concurrent mutation; local actions are still
unsandboxed. There is no promise of offline operation, crash-durable extracted
file contents, or compatibility with Bazel's on-disk cache format.

Neither `--repository_disable_download` nor `--nofetch` is implemented. In upstream
Bazel these do **not** prohibit registry requests, so neither is advertised as an
offline mode here. Full lockfile, extension/repository-rule, toolchain, configured
analysis and application build/test compatibility remain separate requirements.

## Captured validation

`artifacts/remote-validation.json` passes 17 checks: three differential groups
(tar, tar.gz, ZIP query/source build/genrule artifacts) against SHA256-pinned
Bazel 9.3, and fourteen separately labelled native-only checks. The latter cover
warm caches, corruption repair, registry fallback/errors, lazy source fetching,
integrity/path failures and the two reviewer regressions. They are not negative
diagnostic parity claims. HTTP request logs, raw command results, artifact hashes
and HTTP/reference/scratch cleanup receipts are retained in that report.

The full check passes 328 Rust tests and 143 Python harness tests, strict Clippy,
formatting, release compilation and rust-analyzer error diagnostics. The existing
local/custom/loading/module suites remain passing; their reports record their own
binary hashes rather than implying all were rerun after archive-only fixes.

The source-preserving HTTPS application probe reaches `rules_swift@3.1.2` and
fails at unsupported `use_repo_rule`. See `artifacts/mainrepo-remote-loading.json`.
This is progress past missing remote registries, not application build/test
compatibility or evidence of a whole-application speedup.
