# ctx.expand_location, interned Targets, label-keyed dict merging (N50b)

On integration-15 the implementation is N53's LocationExpander
(`src/location_expander.rs`, `analysis::location_map`; docs/MAKE_VARIABLES.md
"Location functions") shared with genrule commands; N50b contributes the
`targets` parameter checks below (absent = `[]`, explicit None a type error),
the interned Targets, the label-keyed dict merging and this receipt. Both
receipts' native sides replay with their recorded verdicts.

## ctx.expand_location(input, targets = [], *, short_paths = False)

StarlarkRuleContext.expandLocation (`analysis/starlark/StarlarkRuleContext.java:1029-1053`):

1. `targets` must be a sequence (`in call to expand_location(), parameter
   'targets' got value of type 'NoneType', want 'sequence'`) of Targets
   (Sequence.cast: `at index 0 of targets, got element of type string, want
   Target`).
2. makeLabelMap (`:1284-1318`) keys each Target by its dependency label
   (AliasProvider.getDependencyLabel: the requested alias, else the label);
   a repeated label is `Label "//pkg:x" is found more than once in 'targets'
   list.` The value is the FilesToRunProvider executable when there is one,
   `--incompatible_locations_prefers_executable` holds (its default) and the
   files to build are not a singleton; otherwise the files to build.
3. `short_paths = True` checks the private-API allowlist (`file
   '//loc:defs.bzl' cannot use private API`; rules_shell is allowlisted).
4. LocationExpander (`analysis/LocationExpander.java:149-470`), built with
   withExecPaths / withRunfilesPaths: the location map is the label map plus
   buildLocationMap (`:410-485`) — the rule's predeclared output files, each
   `srcs` (label list) prerequisite's files to build under its dependency
   labels (alias and actual), and the executable (else the files to build)
   of each `deps`, `implementation_deps` and `tools` label-list
   prerequisite. `data` is not included (allowData is false for Starlark).
   Entries of one label are a set.

Functions (`:368-391`): `location(s)` (exec paths, runfiles paths with
`short_paths`), `execpath(s)`, `rootpath(s)` (runfiles path, `../repo/...`
for external files), `rlocationpath(s)` (`_main/pkg/f`, `repo/pkg/f` for
external files). The argument (trimmed) is parsed with the rule's package
and repository mapping. Paths are sorted, made callable (`./f` for a
single segment), shell-escaped and joined with spaces. Text without a known
function name or without a space after `$(name` is kept.

Errors are **rule errors** (RuleErrorReporter, `:524-535`): `in KIND rule L:
unterminated $(location) expression`, `invalid label in $(location)
expression: ...`, `label '//pkg:x' in $(location) expression is not a
declared prerequisite of this rule`, `... expands to no files`, `label
'//pkg:x' in $(location) expression expands to more than one file, please
use $(locations //pkg:x) instead.  Files (at most 5 shown) are: [a, b]`.
The function name in them is `$(location)` or `$(locations)` whatever was
called. The input is returned unchanged, the implementation continues, and
the target fails after it with every reported error (like
ctx.expand_make_variables). Genuine rules_shell `sh_binary` `env` values
(`ctx.expand_location(value, ctx.attr.data, short_paths = True)` then Make
variables) expand.

## Interned Targets

Every consumer gets the same `Target` value for a dependency configured
target and requested label (Bazel hands out one ConfiguredTarget instance
per label and configuration; an alias is its own
AliasConfiguredTarget): `ctx.attr.deps[0] == ctx.attr.lk.keys()[0] ==
ctx.attr.single`, while an alias and an exec-configured edge differ.
Equality is identity; the hash (label) is consistent with it. A Starlark
transition's configuration is identified per attribute. Since N55 the
identity is per ConfiguredTargetKey for the whole analysis (not just within
a rule context): a Target carried in a dependency's provider, a depset
TargetItem snapshot taken by another target and two attributes with the
same Starlark transition all see the one shared instance
(docs/SH_BINARY.md, tests/target_identity.rs).

## Label-keyed dict concatenation

`select() | {...}`, `{...} | select()` and `select() | select()` merge with
DictType.concat (`packages/Type.java:603-609`, ImmutableMap.Builder
buildKeepingLast): the last value of a label wins and the label keeps the
position of its first occurrence (capture rows B1/B2). Two spellings of one
label inside one dict fail at **loading**, also in a select() branch that
is never selected: `//lkdup:unselected_branch_dup: duplicate labels in each
branch in select expression of attribute 'm' of 'lk' (including
'//lkdup:opt'): //lkdup:p1 (as [":p1", "//lkdup:p1"])`. Different spellings
across concatenated pieces are merged, never reported.

`dict(label, string)` is the attribute type's Type.toString, as needed by
ImplicitOutputsFunction's `Attributes of type dict(label, string) cannot be
used in an outputs substitution template` (N46; on integration-15 tested by
`tests/label_keyed_string_dict_merge.rs`).

## Evidence

`tests/expand_location.rs` (offline, genuine rules_shell sources) and the
paired capture `artifacts/expand_location-capture.py` (genuine rules_shell
from the builtin registry, full builtin closure), receipt
`tests/fixtures/expand_location/reference_receipt.json`, run 1: 12 EXACT
(both builds incl. `-c opt` merges, the interning probe, sh_binary env, all
LocationExpander rule errors, the select-branch duplicate), 4 EXACT_MESSAGE
(EvalException traceback framing: duplicate/None/type targets, short_paths
private API), 1 EXPECTED_NATIVE_UNSUPPORTED (`bazel run //sh:envloc`, N23f;
the genuine output and runfiles tree are in the receipt; re-paired natively on
integration-15, see the receipt's `post_capture_changes`). An undeclared
label in sh_binary `env` yields Bazel's two errors: the location rule error
and then Make expansion's `$(rlocationpath) not defined`.

N55's analysis-wide identity (one Target value per configured target) is
verified by `tests/target_identity.rs` and its paired capture
`artifacts/target_identity-capture.py`, receipt
`tests/fixtures/target_identity/reference_receipt.json`: 2 EXACT (the
15-row identity report — provider-carried Targets, depset snapshots across
producers, same-transition attributes, alias/exec distinct,
`cc_internal.freeze` of Target-holding lists/dicts — and the frozen-list
mutation error), 1 EXPECTED_NATIVE_GAP (Bazel's shallow freeze keeps a
mutable element mutable; the native profile fails explicitly).
