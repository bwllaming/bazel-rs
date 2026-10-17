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

[repr/str/debugPrint patches](REPR.md) for Bazel 9.3 value printing modify
`src/values/types/{structs/value,function}.rs`, `src/environment/globals.rs`,
`src/values/types/string/{interpolation,dot_format}.rs`,
`src/eval/compiler/def.rs`, `src/values/{traits,layout/vtable}.rs` and
`src/stdlib/{extra,funcs/other}.rs` and `src/values/error.rs` (Bazel's
`unhashable type: 'T'` text); each carries an inline modification notice and is
declared in the same provenance with its original archive hashes. The crate's
own tests follow those behaviours: `src/environment/modules.rs`,
`src/values/layout/value.rs`, `src/tests/{basic,uncategorized,fstring,opt/def_inline}.rs`,
`src/values/types/set/{methods,set,value}.rs`, the `format` doc test in
`src/values/types/string/methods.rs`, `testcases/eval/go/function.star` (inline
notices) and 29 `.golden` files regenerated with
`STARLARK_RUST_REGENERATE_GOLDEN_TESTS=1` (listed in `BAZEL_RS_MODIFICATIONS.md`,
whose declared-addition hash is updated). Under the frozen isolated test graph
the vendored crate passes 964 lib tests and 157 doc tests (17 ignored).

The [runfiles increment](RUNFILES.md) (N23a) adds
`StarlarkValue::unknown_attribute_message(attribute)` (default `None`) in
`src/values/{traits.rs,layout/vtable.rs}`; attribute reads
(`src/eval/compiler/expr.rs`, `get_attr_no_attr_error`) and
`Value::get_attr_error` (`src/values/layout/value.rs`) use it before the
ordinary `has no attribute` error, so `ctx.outputs` and `ctx.executable` report
Bazel's `Structure.getErrorMessageForUnknownField` texts
(`analysis/starlark/StarlarkRuleContext.java:402-408`,
`analysis/starlark/StarlarkAttributesCollection.java:82-86`,
`packages/StructImpl.java:86-95`). `src/eval/compiler/expr.rs` is declared
with its original archive hash in the same provenance.

The [N26 string increment](STRING_METHODS.md) patches `src/eval.rs`,
`src/values/types/string.rs` and `src/values/types/string/methods.rs`:
`Evaluator::eval_module` rewrites string literal values per Bazel 9.3's lexer
(`net/starlark/java/syntax/Lexer.java:280-430`) before compilation (raw strings
keep the backslash before a quote; in the default analysis model a literal with a
`\200`..`\377` byte escape keeps lone bytes as private-use characters
U+10FF00 + byte, `eval::bazel_byte_char`), a per-thread
Latin-1 char model (`enter_latin1_char_model`, re-exported from
`values::string`) selects Bazel's Java char semantics for `isspace`, `title` and
`capitalize`, and `split`/`rsplit` require a string `sep`
(`StringModule.java:387-468`) while `elems()` returns a list (`:926-940`).
Upstream doc examples and one unit test that asserted the replaced behaviour were
updated with modification notices; three tests were added. The isolated vendored
test run (frozen h1 test-graph lock, direct 1.99.0 binaries, offline;
`/tmp/opencode/n26v-scratch/receipt.json`) showed, on N26's own base, the same
five pre-existing lib failures as its unmodified base tree (whose vendored tests
still expected the pre-N20 `Immutable` text) and no new failure. On the merged
tree (with the N20 vendored test expectations, N22, N23a and N26) the same
procedure passes all 967 lib tests and 157 doc tests (17 ignored).

[Starlark transitions](CONFIGURATION.md) (N18c) use the same N23a hook (the
N18c branch carried identical hunks; one copy is kept). N18c additionally makes
`getattr(x, name)` without a default consult the hook
(`src/stdlib/funcs/other.rs`), as Bazel's `Starlark.getattr` uses
`Structure.getErrorMessageForUnknownField`
(`net/starlark/java/eval/Starlark.java:1002-1008`). The transition `attr`
struct uses it for Bazel's `No attribute '%s'. ...` texts with
`Available attributes: ...`. The patched file carries an inline notice and its
current hash is updated in the provenance; no vendored test changes.

The [N33 interpreter-wording increment](#n33-interpreter-error-texts) gives
the most common interpreter errors Bazel 9.3's `net.starlark.java` texts.

### N33 interpreter error texts

Sources are the pinned Bazel tree (`src/main/java/net/starlark/java/eval/`):

- **`def` binding** (`StarlarkFunction.java:385-523`): `NAME() does not accept
  positional arguments, but got N`, `NAME() accepts no more than K positional
  argument(s) but got N`, `NAME() got unexpected keyword argument(s): a, b` with
  ` (did you mean 'x'?)` only for exactly one, `NAME() missing N required
  positional|keyword-only argument(s): a, b` (all listed) and `NAME() got
  multiple values for parameter 'x'`. `NAME` is the def's own name (`lambda` for
  lambdas). Bazel's order is replayed: the call adds explicit positional
  arguments, explicit named ones, `*args` elements, then `**kwargs` entries
  (`Eval.java:716-756`); duplicates fail immediately; then the positional count,
  unexpected names and missing parameters (`call()`). As in Bazel, `*args`
  elements bind after the named arguments and overwrite a parameter already
  given by name (`g(a = 1, *[2])` binds `a = 2`; `addPositionalArg` has no
  duplicate check, StarlarkFunction.java:460-472, BuiltinFunction.java:307-319);
  only explicit positional arguments and `**kwargs` entries clash.
- **Builtin binding** (`BuiltinFunction.java:150-160, 307-430, 470-490`):
  `NAME() got unexpected keyword argument 'x'` (+ hint over all parameters),
  `NAME() got named argument for positional-only parameter 'x'`, `NAME() got
  multiple values for argument 'x'` / `for keyword argument 'x'`, `NAME() got
  unexpected positional argument`, `NAME() accepts no more than ...`, `NAME()
  missing N required positional|named argument(s): ...`. `NAME` is the method
  name. Positional-only builtins bound by generated code
  (`Arguments::optional`/`no_named_args`), `set()` and `dict()` are rebound
  against their spec on the error path (`ParametersSpec::bazel_rebind_error`).
  A parameter of the wrong type gives `in call to NAME(), parameter 'P' got value
  of type 'T', want 'W'` (`BuiltinFunction.java:404-410`); like Bazel's
  `checkParamValue`, a binding error is replayed with each argument's class
  checked as it binds, so a wrong type wins over the positional count
  (`len(1, 2)`, `depset(1, 2, 3)`; native parameter types are kept on
  `NativeFunc`/`NativeMeth`, with Bazel's declarations for the unchecked `len`,
  `enumerate` and `depset` parameters); `W` derives from the
  starlark-rust type (`str` -> `string`, `None` -> `NoneType`, lists ->
  `sequence`, `string` first, `NoneType` last, `a, b, or c` form,
  `ParamDescriptor.java:122-145`); a container of the right type with wrong
  elements keeps the detailed text. `list`, `tuple`, `set`, `sorted`,
  `reversed`, `any` and `all` check their iterable like Bazel, and `len`,
  `hash`, `getattr`, `hasattr`, `enumerate`, `reversed`, `sorted`, `any` and
  `all` use Bazel's parameter names. All string, list and dict methods and the
  `MethodLibrary` builtins were audited against Bazel's `@Param` names and
  kinds (StringModule.java, StarlarkList.java, Dict.java, MethodLibrary.java):
  `find`/`rfind`/`index`/`rindex`/`count`/`startswith`/`endswith` take a
  positional-only `sub` (so `sub = ` and `needle = ` both fail like Bazel),
  `partition`/`rpartition` `sep`, `join` `elements` (with Bazel's `expected
  string for sequence element N` text), `range(start_or_stop, stop, step)`,
  list `append`/`insert` `item`, `extend` `items`, `index`/`remove` `x`, `pop`
  `i`, `tuple`/`list`/`str`/`repr`/`float`/`type` `x`, `int` a required `x`;
  `dict.get`/`setdefault` take `default` by name, `sorted`'s `key` and
  `enumerate`'s `list` may be positional or named, and `fail` (`msg`, `attr`,
  `sep`, `stack_trace`) and `print` (`sep`) gain Bazel's named-only
  parameters. Approximation: `fail(stack_trace = False)` is accepted, but the
  native error still prints its traceback (Bazel omits it,
  `MethodLibrary.java:891-914`).
- **Call arguments** (`Eval.java:735, 749, 754`): `argument after * must be an
  iterable, not T`, `argument after ** must be a dict, not T`, `keywords must be
  strings, not T`.
- **Fields** (`Starlark.java:1002-1017`, `EvalUtils.java:526-535`):
  `'T' value has no field or method 'x'` with `SpellChecker.didYouMean` over the
  sorted `dir(x)` (a port in `src/errors/did_you_mean.rs`); `struct()` values,
  custom provider instances and `ToolchainInfo` list `Available attributes:`
  instead (`StructImpl.java:85-95`); `cannot set .x field of T value`, and
  `T value does not support field assignment` for structures.
- **Operators**: `unsupported comparison: T1 <=> T2` (`Starlark.java:517`),
  `unsupported binary operation: T1 OP T2` (`EvalUtils.java:416-417`, also for
  `in`), `unsupported unary operation: OPT` (`:472`), `type 'T' has no operator
  [](K)` (`:498-500`), `'T' object is not callable` (`Starlark.java:904`), `type
  'T' is not iterable` (`:284`), `invalid slice operand: T` (`:672`), `'in
  <string>' requires string as left operand, not 'T'` (`EvalUtils.java:382-386`)
  and `dictionary expression has duplicate key: REPR` (`Eval.java:650`).
- **Ordering** (`Starlark.java:492-518`): only bool, int/float, string,
  list, tuple, `Label` and `File` (Artifact by exec path, `Artifact.java:134-145,
  197-199`; `src/starlark_types.rs`) are ordered; `struct()` values are no
  longer ordered (`unsupported comparison: struct <=> struct`), like provider
  instances, depsets, functions, dicts, sets, ranges and `None`.
- **Names**: `name 'x' is not defined` with the SpellChecker hint
  (`Module.java:250-256`, `syntax/Resolver.java:941-952`). Approximation: Bazel
  iterates its candidates in `HashSet` order and keeps the first of equally
  close names; natively the candidates are sorted, so a tie resolves to the
  first in sorted order, which may differ from Bazel's choice.
- **One spelling checker**: the SpellChecker port (`bazel_suggest`,
  `bazel_did_you_mean`, `bazel_edit_distance`, over UTF-16 code units like Java
  strings) is public in `starlark::errors`; bazel-rs uses it for native rule
  attributes, `extension_metadata`, guarded globals, target suggestions and
  option suggestions instead of its former local ports (one of which counted
  UTF-8 bytes).
- **Sequences**: Bazel's `Sequence` includes ranges (`RangeList`); `depset`'s
  `direct`/`transitive` and `Args.add_all`/`add_joined` `values` accept
  `range(...)` (`src/starlark_types.rs` `bazel_sequence`). Parameters whose
  elements must be strings, labels, files or providers still report a
  container error for a range where Bazel reports the element.

Every changed vendored file carries an inline notice and is declared in the
provenance (103 declared patches, 30 new); the three regenerated goldens are
listed in `BAZEL_RS_MODIFICATIONS.md`. The crate's own tests that asserted the
replaced texts now assert the Bazel texts. Under the frozen isolated test graph
(`/tmp/opencode/n33-vendor-tests/run.py`, the N22 procedure: copied tree, frozen
h1 test lock, direct 1.99.0 binaries, offline) the vendored crate passes 968 lib
tests and 157 doc tests (17 ignored). A paired capture against genuine Bazel
9.3.0 (`tests/fixtures/interpreter_wording/reference_receipt.json`) matched 113 of
121 load-time probes in run 1; after the follow-up (ordering, `*args` binding,
binding-time type checks, parameter names, a fixed probe) run 2 matches all
168 probes (157 identical messages, 11 identical `print` outputs; run 1 is kept
in `run_history`, no changes after run 2). `File` ordering is covered by unit
tests only (the capture evaluates at load time). Static syntax errors from `starlark_syntax` (`repeated named
argument`) and the traceback layout are unchanged.

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
