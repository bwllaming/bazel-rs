# Local modifications by bazel-rs

This notice is a local addition, not an upstream NOTICE file.

The profiling data file
`src/eval/runtime/profile/golden/bytecode.csv.golden` was modified to add the
zero-count `SubAssign`, `BitAndAssign` and `BitXorAssign` rows introduced by the
assignment-specific set operations. Its remaining rows and counts are unchanged.
This separate notice preserves the machine-readable CSV format.

The error-display golden files `src/tests/uncategorized_error_display.golden`
and `src/tests/uncategorized_error_display_hash.golden` (plain and alternate
`{:#}` rendering of the same error) were modified to expect Bazel 9.3's frozen
list mutation text, `error: trying to mutate a frozen list value`
(`Starlark.checkMutable`), instead of `error: Immutable`. Their generated
headers and all other lines are unchanged; the comparison format cannot carry
an inline notice.

Modified Rust sources carry inline modification notices. Their original and
current hashes, and this added notice, are recorded in the adjacent
`../starlark-0.14.2.provenance.json`. Original upstream license and copyright
notices remain intact.
