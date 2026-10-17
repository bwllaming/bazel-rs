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

For native Bazel 9.3 repr compatibility (see `docs/REPR.md` of bazel-rs), the
following golden files were regenerated with the crate's own mechanism
(`STARLARK_RUST_REGENERATE_GOLDEN_TESTS=1`). Their only changes are the printed
names of callables and values: native functions as `<built-in function NAME>`,
`def`s as `<function NAME from FILE>`, `struct(a = 2)` struct spacing and the
`builtin_function_or_method` type name. Golden contents are compared verbatim,
so this separate notice replaces inline comments:

- `src/eval/runtime/profile/golden/heap_allocated.csv.golden`
- `src/eval/runtime/profile/golden/heap_allocated.flame.golden`
- `src/eval/runtime/profile/golden/heap_flame_allocated.flame.golden`
- `src/eval/runtime/profile/golden/heap_flame_retained.flame.golden`
- `src/eval/runtime/profile/golden/heap_summary_allocated.csv.golden`
- `src/eval/runtime/profile/golden/heap_summary_retained.csv.golden`
- `src/eval/runtime/profile/golden/time_flame.flame.golden`
- `src/tests/bc/golden/call.golden`
- `src/tests/bc/golden/def_inline_const_args_inlined.golden`
- `src/tests/bc/golden/def_inline_locals_inlined.golden`
- `src/tests/bc/golden/def_inline_return_type_inlined.golden`
- `src/tests/bc/golden/definitely_assigned_mov_is_used.golden`
- `src/tests/bc/golden/definitely_assigned_slot_range_in_call.golden`
- `src/tests/bc/golden/eq_const.golden`
- `src/tests/bc/golden/for.golden`
- `src/tests/bc/golden/for_break.golden`
- `src/tests/bc/golden/for_continue.golden`
- `src/tests/bc/golden/if_stmt_and_stmt.golden`
- `src/tests/bc/golden/if_stmt_if_false_or_x.golden`
- `src/tests/bc/golden/if_stmt_if_true_and_x.golden`
- `src/tests/bc/golden/if_stmt_if_true_or_x.golden`
- `src/tests/bc/golden/if_stmt_if_x_and_true.golden`
- `src/tests/bc/golden/if_stmt_if_x_or_false.golden`
- `src/tests/bc/golden/if_stmt_if_x_or_true.golden`
- `src/tests/bc/golden/if_stmt_or_stmt.golden`
- `src/tests/bc/golden/opt_list_of_expr_add.golden`
- `src/tests/bc/golden/opt_mutual_recursion.golden`
- `src/tests/bc/golden/opt_recursion.golden`
- `src/typing/tests/golden/incorrect_type_dot.golden`

For Bazel 9.3 interpreter error texts (N33: argument binding, unknown fields,
operators and undefined names, see `docs/STARLARK_VENDOR.md` of bazel-rs), the
following golden files were regenerated with the crate's own mechanism
(`STARLARK_RUST_REGENERATE_GOLDEN_TESTS=1`); only their `error:` lines change
(`src/typing/tests/golden/incorrect_type_dot.golden`, listed above, also has its
two `error:` lines changed to `'builtin_function_or_method' value has no field
or method 'foo'`):

- `src/tests/fstring/golden/undeclared_variable.err.golden.md`
  (`error: name 'bar' is not defined`)
- `src/values/types/record/record_type/missing_field_error.golden`
  (`error: RecFail() missing 1 required named argument: b`)

For Bazel 9.3's rejection of recursive calls (N48, `function 'NAME' called
recursively`, see `docs/STARLARK_VENDOR.md` of bazel-rs), function bodies
whose calls may run Starlark code (here `noop(y, x)`) are no longer inlined,
and the following golden files (listed
above) were regenerated with `STARLARK_RUST_REGENERATE_GOLDEN_TESTS=1`: their
test comment (formerly "This call should be inlined.") now says the call is
not inlined, and their bytecode calls `foo` (`CallFrozenDefPos`) instead of
its inlined body:

- `src/tests/bc/golden/def_inline_const_args_inlined.golden`
- `src/tests/bc/golden/def_inline_locals_inlined.golden`

Modified Rust sources carry inline modification notices. Their original and
current hashes, and this added notice, are recorded in the adjacent
`../starlark-0.14.2.provenance.json`. Original upstream license and copyright
notices remain intact.
