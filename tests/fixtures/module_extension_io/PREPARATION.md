# Module extension I/O genuine reference-only collector

`python3 scripts/validate_module_extension_io.py --plan` emits concrete case and
reference command templates; counts come from the code. It launches nothing.
`--reference-only` requires explicit `--reference`, `--reference-sha256`,
`--registry-input`, `--official-source`, and a new owned `--artifact-dir` beneath
`/tmp/opencode/module-extension-io-*`. It freezes stable inputs, captures once and
continues independent semantic mismatches as UNMATCHED. Health/isolation/input
drift hard-stops with an EXECUTION_BLOCKER receipt and cleanup. There is no retry.
`--native` and `--native-sha256` expose the future full-mode interface but fail
closed before any effect. No in-flight Rust/native source is read or claimed bound.

Before the API matrix, `--setup-only` runs one separate simple file/action build
and cquery with `--repository_disable_download` and an empty private checksum
cache. The matrix additionally requires `--setup-receipt`,
`--setup-freeze-sha256`, and `--setup-archive-sha256`; failed setup cannot launch it.
The setup receipt must prove cold private-cache absence, pinned platform files,
source mode 0644, completed output mode 0555, exact cquery/action paths and hashes,
and zero HTTP requests. A setup-stage discrepancy stops before the 33 API cases.

The only downloader repair is an exact scheme-stripped host/path rewrite for the
pinned platforms 1.0.0 release to owned loopback `/__setup__/platforms-1.0.0.tar.gz`.
The rewriter deliberately strips schemes before matching (UrlRewriter:262-296);
the genuine input release URL is pinned HTTPS. No public host is allowlisted.
DownloadManager:280-287 rejects an empty rewritten list before distdir, while
289-324 verifies/copies distdir before the disable check at331. Retaining a valid
loopback URL with the original basename makes that verified reuse eligible.
Staged archive size7879/SHA3384eb… are rechecked at every launch checkpoint.
Any actual platform request is tagged `platform-setup`, preserved and checked
against the explicit zero-request plan separately from `ctx-api`; none is ignored.
The first 33-UNMATCHED capture remains immutable and is never reclassified.

The subsequent zero-HTTP setup failure is also immutable. Pinned `utils.bzl:238-248`
identifies its denied download as the **implicit registry MODULE.bazel**, not a
declared version patch: `IndexRegistry:536-538` always supplies that file and its
SHA256 SRI (`RegistryFileDownloadEvent:43-49`). Existing tracked platforms1.0.0
source.json declares no explicit patches/overlays. `platform_closure.json` pins
these sources and the genuine tracked286-byte module file. No canned MODULE or
invented version patch is used. The helper validates every declared patch/overlay
and the implicit module file; captures use only the exact pinned tracked metadata.

The archive stays in its existing private distdir. Each additional governed file
has a SHA-keyed distdir, preventing basename collisions. All directories are
passed to Bazel, and every prerequisite's exact SRI/bytes is checked before each
launch. The module-file URL stays the genuine private registry `file:` URL:
UrlRewriter does not filter non-HTTP(S), and eligible verified distdir reuse still
precedes the disable check. `--official-tree` supplies the separately hashed
immutable Java/repository-rule source files; current native sources are excluded.

After a disabled platform-closure setup PASS, `--api-smoke-only` requires its pinned
receipt and runs the original checksummed-json cold/warm/cquery case with the
disable flag absent. This is actual module_ctx download/read -> repository ->
consumer proof, not static platform proof. Full `--reference-only` additionally
requires `--api-smoke-receipt`, `--api-smoke-freeze-sha256` and
`--api-smoke-archive-sha256`. Either smoke failure stops before the33-case matrix.

`versions.json` is the exact 620-byte original public input. `contract.json`
records compact facts and original report/manifest pins from the four immutable
captures under `/tmp/opencode/bazel-module-download-contract`. Those absolute
archives are attribution only, not portable-test dependencies. The two original
failed expectations remain failed. Boundary bodies are `bytes(i % 251 for i in
range(n))`, as in the captured supplemental producer. The fifth SRI algorithm is
BLAKE3 (pinned `Checksum.java:102-105`); its empty-input test vector is explicit,
not a new Python hashing implementation. Only SHA-256/SHA-512 results were already
captured; the other algorithm cases are planned, not executed proof.

The fixture compiler uses real `module_ctx.download` and `read`; successful values
flow through a real repository declaration, repository_rule materialization and a
custom action consumer. The checksummed JSON case actually parses versions/archive
records. There is no fake host, canned result or injected provider global. The
frozen runtime report, not this fixture description, determines executed matches.
All256 callback strings represent Latin-1 bytes; native UTF-8 encoding of the
same logical string is distinct from the reference's raw Latin-1 diagnostic stream.

## Live and replay validation

`validate_stage` and `validate_completed_case` are shared LIVE/REPLAY predicates.
`--replay-reference` requires `--artifact-dir`, `--reference`, externally supplied
`--freeze-sha256` and `--archive-sha256` (SHA256SUMS.json's hash). It binds the full
archive/report, current QA and reference ELF, exact complete chronology, source
sites, typed callback operations, request order, actual cquery/action/file paths,
root entries and independent consumer bytes/modes. It recomputes case verdicts;
an UNMATCHED archive can replay correctly but cannot become a MATCHED case.
All raw streams remain byte-for-byte archived. Reference JSON frames are decoded
as Latin-1 only after exact DEBUG source-path/line/column binding; source echoes
are not markers. Planned negatives require the exact source-bound traceback and
diagnostic, not a nonzero status or token. Native footer handling is not enabled.
The older generic `verify_record`/`replay` helpers remain portable seal tests only.

## Capture isolation and limits

* Freeze concrete origins, private roots, public input bytes, scripts and
  executables; record every auxiliary event and the complete chronology. Event
  rows are explicitly distinct from subprocess commands. Source binding includes
  only QA/fixtures, copied registry, platform archive, pinned official source
  copies and direct reference ELF, deliberately excluding changing native files.
* Owned loopback listeners/process groups; private HOME/TMPDIR, fixed
  PATH/LC_ALL, rc ignored, local copied registry and genuine downloader allowlist.
  No global trust, credentials, proxies or TLS bypass. Preserve request order.
* Verify and reuse tracked `../label_repo_name/platforms_archive.json` into private
  `--distdir`; its bytes are checked against the fixed size and SHA, not its stale
  historical source path. Never fetch the platforms release externally.
* Do not pass `repository_disable_download` except intended cases. This genuine
  flag is default-off and is not an all-network switch: metadata/Git remain outside.
* Retain raw streams plus byte-preserving logical readbacks. Bind action paths to
  actual fixture/repository/cquery provenance, not basenames. Collect cold/warm
  consumer hashes, action mode **0555**, cache and publication independently of
  callback prints. Download source mode and existing **0640** are separate.
  Preserve the root directory entry `""`, 0755, size 0, SHA-256(empty).
* Cold/negative callbacks run exactly once. Warm/cquery explicitly permit zero or
  one complete callback, never partial or extra callbacks; request counts follow
  that exact sequence. Cold requires one actual consumer action; warm requires
  none. Cquery must resolve both the actual repository input and consumer output.
* Command timeout defaults to 90s (maximum 120s); aggregate 1200s, 512 requests,
  1536 MiB scratch. Stream size exceeding 8 MiB is a hard health failure, not a
  semantic negative. Every command has before/after pins and resource checkpoints.
  Server/process groups and evaluator working paths are checked and scratch is
  removed even after failures. The full public inputs and raw outputs survive.

`deferred.json` separates reference-only path/execute instrumentation, native
unit/context proof, unsupported-feature zero-effect checks and TLS limitations.
Local unchecksummed HTTPS native CLI proof is unavailable without a real supported
test trust path. Private reference JVM trust and injected native real-transport
unit proof are separate lanes. A public application HTTPS probe belongs to the
parent later. This capture uses checksummed loopback HTTP and zero-request
unchecksummed-HTTP filtering; it does not replay the prior private-JVM HTTPS lane.
The 32-KiB/0640/truncation observations remain immutable prior reference facts,
not new runs or native CLI-equivalent instrumentation. Native resource policies
remain a separate unfrozen core gate. Nothing
here requires SDK/toolchain archives, facts, persistent watches or API completeness.
