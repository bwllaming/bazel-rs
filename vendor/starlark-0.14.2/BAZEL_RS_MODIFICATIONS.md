# Local modifications by bazel-rs

This notice is a local addition, not an upstream NOTICE file.

The profiling data file
`src/eval/runtime/profile/golden/bytecode.csv.golden` was modified to add the
zero-count `SubAssign`, `BitAndAssign` and `BitXorAssign` rows introduced by the
assignment-specific set operations. Its remaining rows and counts are unchanged.
This separate notice preserves the machine-readable CSV format.

Modified Rust sources carry inline modification notices. Their original and
current hashes, and this added notice, are recorded in the adjacent
`../starlark-0.14.2.provenance.json`. Original upstream license and copyright
notices remain intact.
