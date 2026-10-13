# Pinned Starlark vendor baseline

The root `[patch.crates-io]` selects `vendor/starlark-0.14.2`. The dependency
remains exactly `starlark = "=0.14.2"`; the lockfile changes only by removing
that package's registry source and checksum. This baseline does not enable
`set`, change Starlark semantics, or upgrade any dependency.

## Original published contents

The source archive is
<https://crates.io/api/v1/crates/starlark/0.14.2/download>, SHA-256
`9062e866918dc4c9701c98ac99f7f4fa9e4b3b4edce306e147393bc75458c4fc`.
Its `.cargo_vcs_info.json` identifies upstream commit
`0c8d107ab963362ba47ee3ab9e0173bc39bd101d` in
<https://github.com/facebook/starlark-rust>.

All 685 regular archive members (3,535,949 bytes) retain their original relative
paths and exact contents, including the published normalized `Cargo.toml`,
`Cargo.toml.orig`, VCS metadata, upstream tests and golden files. There are no
omitted members or source patches. No formatting, lint suppression, feature
change, or pruning is permitted in this baseline.

`vendor/starlark-0.14.2.provenance.json` records the origins and explicitly
distinguishes the additional license from published archive members. Its
archive-member digest binds the complete, lexically ordered per-path size/SHA-256
mapping. It is reproducible directly from the tracked tree; no ignored receipt,
network request or registry cache is required. The optional expanded inventory
is in `artifacts/starlark-set-vendor-baseline.json`.

From the repository root, verify the tracked baseline with:

```sh
python3 - <<'PY'
import hashlib, json
from pathlib import Path
p = json.loads(Path('vendor/starlark-0.14.2.provenance.json').read_text())
root = Path(p['archive']['archive_member_inventory']['path_base'])
extra = {v['path']: v for v in p['additional_files']}
members, found_extra = [], set()
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
        members.append(item)
assert found_extra == set(extra)
assert len(members) == p['archive']['regular_members']
assert sum(v['size'] for v in members) == p['archive']['uncompressed_regular_file_bytes']
raw = json.dumps(members, sort_keys=True, separators=(',', ':')).encode()
assert hashlib.sha256(raw).hexdigest() == p['archive']['archive_member_inventory']['canonical_json_sha256']
print('Exact baseline inventory and added license verified')
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

## Verification and scope

The extraction inventory rejects absolute paths, parent traversal, duplicate
paths, links and special files before copying any member. Verification compares
every vendored member directly with its archive bytes and rejects unexpected
files except the separately hashed LICENSE addition.

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

## Verified baseline results

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
