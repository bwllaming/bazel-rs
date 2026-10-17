# Module-extension download and read

**Superseded by N43.** `module_ctx.download`, `download_and_extract`, `read`,
`path`, `file`, `extract`, `execute`, `which`, `watch` and `report_progress` are
the shared StarlarkBaseExternalContext implementation of `repository_ctx`
(`src/repository_ctx.rs`) running in the extension's working directory
`<cache>/modextwd/<unique name>`; see
[EXTENSION_CTX_SURFACE.md](EXTENSION_CTX_SURFACE.md),
[REPOSITORY_RULES.md](REPOSITORY_RULES.md) and
[REPOSITORY_IO_STREAMING.md](REPOSITORY_IO_STREAMING.md). The former bounded
"owned scratch" profile (owned-file identity checks, the 32 KiB checksum
precheck window, rejected `executable`/`block=False`/`canonical_id`/`auth`/
`headers`, versioned `extension_io` observations of every download and read)
was removed: Bazel records neither downloads nor working-directory reads as
extension inputs. Recorded inputs are now `extension_inputs` (version 2:
watched `files` and `host_facilities`) in the extension provenance.

The validator `scripts/validate_module_extension_io.py` (and its native
collector `tests/fixtures/module_extension_io/native_capture.py`) is retired
(N43b): it is no gate or evidence, refuses every mode except `--plan`, and its
stored receipts describe the removed model only. Its 33 recipes are replayed on
genuine Bazel 9.3 and native by `artifacts/ext_ctx-capture.py --targeted-n43b`
(`tests/fixtures/ext_ctx_surface/reference_receipt.json`, `targeted_rerun_n43b`).

The evidence notes below are historical (they describe the removed profile).

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

Current implementation authority is [the shared context](../src/repository_ctx.rs),
[module_ctx](../src/extension_ctx.rs),
[transport and checksums](../src/extension_download.rs), [provenance](../src/repository.rs) and
[materializer policy](../src/materializer.rs). Historical plans/receipts do not
widen these supported surfaces.
