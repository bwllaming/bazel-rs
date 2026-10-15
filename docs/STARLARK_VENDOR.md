# Pinned Starlark vendor provenance

The root `[patch.crates-io]` selects `vendor/starlark-0.14.2`. The dependency
remains exactly `starlark = "=0.14.2"`; H0 changed its lock entry only by removing
that package's registry source and checksum. The original H0 baseline changed no
semantics. Subsequent [set-core compatibility patches](STARLARK_SETS.md) are
declared separately; they do not upgrade dependencies. Production globals and
provider-file traversal are enabled by the subsequent native integration, not by
the vendored-core patch itself.

The native JSON codec adds one typed, allocation-free BigInt borrow accessor to
the vendored Value API, permitting budget checks before integer conversion. Its
patch and tests are recorded separately in the same provenance. The root crate
also adds a direct dependency on the already-resolved `num-bigint = "=0.4.8"`;
dependency versions and enabled features remain unchanged.

[String-method and `repr` patches](STRING_METHODS.md) for Bazel 9.3 byte-string
semantics modify `src/values/types/string/{methods,repr,globals}.rs`; they are
declared in the same provenance with their original archive hashes.

[Location-aware indexing and `in`](TOOLCHAINS.md) for native `ctx.toolchains`
string keys add two `StarlarkValue` methods with defaults,
`at_with_location(index, heap, location)` and `is_in_with_location(other,
location)`, which delegate to `at` and `is_in`. `location` lazily returns the
span of the indexing or `in` expression (its own module, also when the
expression was inlined from another module's function). Only the bytecode
`ArrayIndex` and `In` instructions call them (`src/eval/bc/instr_impl.rs`;
`In` becomes a no-flow instruction with the same argument slots); the vtable
gains the two entries (`src/values/layout/vtable.rs`, `src/values/traits.rs`).
Other values keep a single vtable call; the span is computed only when a value
asks for it. An A/B release benchmark of 4,000,000 iterations of list/dict
indexing and `in` over the pristine and patched crates measured 0.326-0.333 s
(pristine) versus 0.303-0.307 s (patched), i.e. no regression. The vendored
crate's own tests cannot run in this workspace (`starlark` is not a member and
needs dev-dependencies); the behaviour is covered by `src/toolchain_info.rs`
unit tests and `tests/toolchains.rs`.

The [label-dictionary increment](ATTR_DICTS_COMPUTED_DEFAULTS.md) (N20) patches
`src/values/{error,traits}.rs`, `src/values/layout/{value,vtable}.rs`,
`src/values/types/{list/value,dict/refs,dict/value}.rs` and
`src/eval/compiler/stmt.rs`: mutating a frozen list or dict reports Bazel's
`Starlark.checkMutable` text (`net/starlark/java/eval/Starlark.java:262-265`), `trying to mutate a frozen list value` /
`trying to mutate a frozen dict value` (other immutable values keep
`Immutable`), and a new `StarlarkValue::rbit_or` hook (default `None`) lets a
non-dict right operand of a dict `|` / `|=` answer first, as Bazel's
`SelectorList.binaryOp(op, that, thisLeft = false)` does for
`{...} | select(...)` (`packages/SelectorList.java:171-182`). These files are
declared with their original archive hashes in the same provenance. The
vendored crate's own tests that asserted the former list/dict text are updated
to the Bazel text as declared patches: `src/tests/call.rs`,
`src/tests/uncategorized.rs` (`test_error_display`),
`src/values/types/dict/methods.rs`, `src/values/types/list/value.rs`, and the
two error-display goldens `src/tests/uncategorized_error_display{,_hash}.golden`
(plain and `{:#}` renderings; only the `error:` line changes, and their notice is
in `BAZEL_RS_MODIFICATIONS.md` because the golden format cannot carry one).
Tuple, bytes and set mutation tests still expect `Immutable`.

## Original published contents

The source archive is
<https://crates.io/api/v1/crates/starlark/0.14.2/download>, SHA-256
`9062e866918dc4c9701c98ac99f7f4fa9e4b3b4edce306e147393bc75458c4fc`.
Its `.cargo_vcs_info.json` identifies upstream commit
`0c8d107ab963362ba47ee3ab9e0173bc39bd101d` in
<https://github.com/facebook/starlark-rust>.

H0 preserved all 685 regular archive members (3,535,949 bytes), including the
published manifests, VCS metadata, tests and golden files. All paths remain
present. Each later source patch records original and current sizes/hashes;
modified files retain their license headers and carry modification notices.
The original archive and inventory digests are never replaced by patched-tree
digests. Unlisted changes or additions fail verification.

`vendor/starlark-0.14.2.provenance.json` records the origins and explicitly
distinguishes the additional license from published archive members. Its
archive-member digest binds the complete, lexically ordered per-path size/SHA-256
mapping. It is reproducible directly from the tracked tree; no ignored receipt,
network request or registry cache is required. The optional expanded inventory
is in `artifacts/starlark-set-vendor-baseline.json`.

From the repository root, verify current files and reconstruct the anchored
original inventory using only declared patches:

```sh
python3 - <<'PY'
import hashlib, json
from pathlib import Path
p = json.loads(Path('vendor/starlark-0.14.2.provenance.json').read_text())
root = Path(p['archive']['archive_member_inventory']['path_base'])
extra = {v['path']: v for v in p['additional_files']}
patches = {v['path']: v for v in p['declared_source_patches']}
assert len(extra) == len(p['additional_files'])
assert len(patches) == len(p['declared_source_patches'])
assert not set(extra).intersection(patches)
members, found_extra, found_patches = [], set(), set()
for f in sorted(root.rglob('*'), key=lambda f: f.relative_to(root).as_posix()):
    assert not f.is_symlink(), f
    if f.is_dir():
        continue
    assert f.is_file(), f
    data = f.read_bytes()
    item = dict(path=f.relative_to(root).as_posix(), size=len(data),
                sha256=hashlib.sha256(data).hexdigest())
    if item['path'] in extra:
        assert all(extra[item['path']][k] == v for k, v in item.items())
        found_extra.add(item['path'])
    else:
        if item['path'] in patches:
            patch = patches[item['path']]
            assert all(item[k] == patch['current'][k] for k in ('size', 'sha256'))
            found_patches.add(item['path'])
            item = dict(path=item['path'], **patch['original'])
        members.append(item)
assert found_extra == set(extra)
assert found_patches == set(patches)
assert len(members) == p['archive']['regular_members']
assert sum(v['size'] for v in members) == p['archive']['uncompressed_regular_file_bytes']
raw = json.dumps(members, sort_keys=True, separators=(',', ':')).encode()
assert hashlib.sha256(raw).hexdigest() == p['archive']['archive_member_inventory']['canonical_json_sha256']
print('Original inventory, declared patches and declared additions verified')
PY
```

## Authentic license addition

The published archive has no LICENSE or NOTICE file, despite its per-file
Apache headers. `vendor/starlark-0.14.2/LICENSE` is an unmodified copy fetched
from the official repository at the exact source commit:

<https://raw.githubusercontent.com/facebook/starlark-rust/0c8d107ab963362ba47ee3ab9e0173bc39bd101d/LICENSE>

It is 11,324 bytes, SHA-256
`50e6751797c50dedd75ef1b8a0d9e42f5f8472e9fbce91f34718e9f97b0c780a`.
This is an explicit addition, not an archive member. The pinned upstream
NOTICE request returned HTTP 404 and the complete pinned tree contained no
NOTICE, so none is fabricated. Acquisition evidence is in the `licensing`
section of `artifacts/starlark-set-edge-contract.json`.

`BAZEL_RS_MODIFICATIONS.md` is a separately declared local notice for the modified
profiling CSV, whose machine-readable format cannot carry a source comment. It
is not an upstream NOTICE file. Rust patches carry inline modification notices.

## Verification and scope

The extraction inventory rejects absolute paths, parent traversal, duplicate
paths, links and special files before copying any member. Verification compares
every original member directly with its archive bytes. The portable verifier
checks declared current patches and reconstructs that original inventory;
unexpected files are rejected except separately declared additions.

Before/after `cargo metadata --locked --offline --format-version 1` receipts
are `artifacts/starlark-vendor-before-metadata.json` and
`artifacts/starlark-vendor-after-metadata.json`. Compare full resolution nodes,
dependency edges and enabled features after normalizing only the Starlark
registry-to-path package ID. All other package metadata and lockfile entries
must remain identical. The workspace must still contain only `bazel-rs`.

Use the repository-local pinned toolchain through `direnv exec .`. The parent
verification gate owns full tests, strict Clippy, formatting, analyzer, release
build, frozen-binary regression surfaces and final review. A dependency-source
transition can change compiled bytes; identical executable hashes are not a
semantic-equivalence requirement. Upstream files must not be reformatted to
make a broader formatting command pass. Record any path-dependency lint or
format-scope failure rather than silently changing the preserved source.

## Verified H0 baseline results (historical)

The baseline passes 952 Rust and 533 Python tests, formatting, strict Clippy,
release and locked/offline builds. Twelve native/reference regression suites pass
on the frozen candidate. Direct comparison with the preserved pre-vendor binary
matches cold/warm/source-edit outputs, cache behavior and invalid-root rejection.
The application checkout and required Git symlink remain unchanged; loading still
stops at missing `set`. No set enablement or performance improvement is claimed.

Analyzer status is **KNOWN_BASELINE_ERRORS**, not clean: vendoring exposes twelve
diagnostics within unchanged dependency files. An isolated, separately extracted
pristine archive with equivalent Cargo configuration reproduces the exact same
diagnostic multiset; a final current-tree scan matches it. All three scans report
zero root-source/test errors. This establishes reproducibility, not the cause of
the diagnostics. Raw scans, metadata equality and cleanup are retained in
`artifacts/h0-analyzer-baseline-comparison.json`.

The original trailing whitespace in `src/values/layout/heap/send.rs:56` is also
preserved byte-for-byte rather than rewriting upstream source. The non-vendor
staged whitespace check passes. Source and report bindings are recorded in
`artifacts/h0-vendor-verification-manifest.json`.
