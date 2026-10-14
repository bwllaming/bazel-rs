# Native JSON compatibility profile

This verified bounded profile uses a dedicated codec, not an enablement of the
interpreter's incompatible JSON extension. It is not unrestricted JSON/Bazel parity.

## Module and values

The immutable `json` module exposes `encode`, `decode`, `indent` and
`encode_indent` in BUILD and both `.bzl` loading modes. Direct MODULE globals do
not include `json`. Primary arguments are positional-only; decode's optional
default accepts a positional or named value. Formatting options are keyword-only.

Decode distinguishes integer lexemes from decimal/exponent floats, preserving
arbitrary integer precision, signed floating zero and overflow infinities.
Encoding rejects nonfinite floats. Nested results are fresh mutable caller-heap
values; duplicate keys retain their first position and last value. Syntax/depth
fallback returns the exact supplied default. Binding, type, internal and quota
failures must not be converted to a default.

Encoding supports primitives, list/tuple/set/range, string-key dictionaries,
ordinary structs and actual custom-provider fields. Keys and field names sort in
UTF-8 order under the pinned default byte-string semantics. Labels, callables,
the JSON module and builtin NativeInfo variants are outside this encoder profile.
No arbitrary `dir/getattr` reflection or generic serde coercion supplies fields.

File encoding is limited to validated main-repository source/generated Files,
with actual `path`, `root` and `short_path` metadata. External or inconsistent
profiles reject explicitly. Native generated-root spelling is retained, not
rewritten to pretend configuration-path equality with the reference.

## Exact formatting and preserved quirks

JSON escaping is independent of general Starlark repr. Valid UTF-8 is preserved;
escaped surrogate pairs decode normally and unmatched halves become U+FFFD.
Diagnostic offsets count original input bytes, including escape spelling.
Rejection categories and these offsets are checked; native error messages do not
yet reproduce all reference type names or nested key/index context. Full diagnostic
text/context parity is not claimed.
The research contract's nested key/index diagnostic-context requirement is
deferred, not implemented or counted as a passing compatibility criterion.

Finite float formatting uses an independently implemented exact-integer
closest-even decimal selection, followed by the pinned general-format layout.
Neither existing native display nor serde formatting is assumed compatible.
The approved prototype matches 1,024 genuine reference strings and bit patterns;
23,440 further checks are native-only properties, not additional reference cases.

Indentation preserves tokens, duplicate keys, number spelling and string escapes;
it is not decode-and-reencode and is not a JSON validator. Pinned harmless quirks
are preserved explicitly: `5.E1` decodes while `5.e1` rejects; numeric input at
EOF can fail indentation while the same input with trailing whitespace succeeds;
`[}` is retained and `null trailing` can truncate to `null`.

## Resource and compatibility bounds

The codec uses method-specific weighted budgets and an explicit 64-open-container
depth profile. It charges before growth, conversion and materialization, including
discarded duplicates and repeated noncyclic aliases. Active-path cycle detection
does not suppress repeated values. The borrowed integer accessor permits size
inspection before copying or formatting existing large integers.

The decode-capacity candidate uses **256 MiB weighted work and 250,000 nodes** for
`json.decode` only, with an explicit **8 MiB effective UTF-8 input ceiling** checked
before scanning. Input bytes, duplicate members, temporary storage, heap copies,
and nonlinear numeric-conversion work are still charged. `encode`, `indent`, and
`encode_indent` retain **64 MiB / 100,000 nodes**; compound helpers share their
original cumulative budget. Every non-JSON `HostBudget::default()` is unchanged.
Resource, arithmetic and allocation failures cannot return a supplied decode
default; only the established syntax/depth cases can do so.

These candidate ceilings require completed-input measurement and independent
acceptance, not merely a larger constant or successful prefix parse. A newly
captured 2,233,984-byte public metadata sample exceeded the old budget and was
accepted by pinned Bazel 9.3; it is not the unavailable original application
response. Routine Rust tests generate deterministic metadata-shaped input and do
not depend on ignored sample artifacts. Successful decode need not imply that
the unchanged encoder or indenter can process the same large value. Budgets are
per call, not a process-wide retained-memory guarantee.

Float arithmetic is finite-domain and bounded, but table, workspace, temporary
and output allocations still require precharging. These accounting bounds are
not claims of an exact JVM recursion threshold, absolute RSS ceiling, universal
all-input performance, or unrestricted JSON/Bazel parity.

Research is recorded in `artifacts/json-contract.json`,
`artifacts/json-edge-contract.json`,
`artifacts/json-float-algorithm-contract.json` (rejected heuristic), and
`artifacts/json-closest-even-prototype.json` (approved exact selector). Portable
test fixtures retain the actual reference cases; ignored reports are not test
inputs.

## Historical initial JSON verification

The combined gate passes **1,104 Rust and 645 Python tests**, including 24 real
CLI groups and 36 portable harness tests. The frozen comparison passes eight
families, 45 probes, 82 phases and 127 commands, including all 1,024 exact float
cases in each engine. Its status is `PASS_ONLY_BOUNDED_PROFILE`; native safety,
File/NativeInfo and diagnostic-context exclusions remain explicit.

The same frozen interpreter test graph passes 957 library tests and 157 doctests,
retaining the same 17 preexisting ignores. Fourteen adjacent suites pass on the
candidate. Analyzer diagnostics retain the exact twelve known dependency errors,
with zero new or root-source errors. Dependency versions/features remain unchanged;
only the approved direct BigInt dependency edge is added.

The first failed comparison is preserved unchanged. Corrected fixtures avoid
unsupported BUILD printing, use anchored engine-specific diagnostics, and validate
the actual content-addressed repository output rather than an assumed symlink.
These corrections do not implement the deferred nested diagnostic context.

The preserved initial application query advanced to `repository_rule(environ=["GOROOT"])`,
which was unsupported at that checkpoint. Checkout contents and the required Git symlink remained
unchanged; all J-owned scratch and reference processes are cleaned. See
`artifacts/json-validation.json`, `artifacts/j-json-verification-manifest.json`
and `artifacts/mainrepo-json-loading.json` for the bound evidence.

This combined tree uses the decode-only capacity profile above together with
module-extension I/O. `module_ctx.read` returns one character per byte (U+0000..U+00FF),
and the 8 MiB decode input ceiling counts UTF-8 bytes, so each byte >= 0x80 counts
twice: a read file of roughly 4 to 8 MiB with many high bytes can exceed the decode
ceiling although the read itself succeeds. In repository/extension evaluation,
`\u` escape runs decode to UTF-8 bytes like Bazel (see MODULE_EXTENSION_IO.md).
Application query, build and test remain unproven until rerun on this tree.
