# rule(outputs = ...) implicit outputs (N46)

Bazel 9.3's deprecated-but-supported `rule(outputs = ...)` predeclared outputs:
a dict of `key -> template` or a Starlark function computing that dict from
the rule's attributes. Paired genuine Bazel 9.3.0 capture:
`tests/fixtures/implicit_outputs/reference_receipt.json` (script
`artifacts/implicit_outputs-capture.py`), workspace
`tests/fixtures/implicit_outputs/workspace`, CLI tests
`tests/implicit_outputs.rs`, unit tests in `src/implicit_outputs.rs`.
Source references are to the pinned Bazel 9.3 tree.

Capture run 2 (final; run 1 kept as artifacts/implicit_outputs-receipt-run1.json,
0 mismatches, rerun after review changes): 60 rows — 17 EXACT, 31
EXACT_MESSAGE, 4 REFERENCE_CRASH_NATIVE_EXPLICIT, 3
EXPECTED_MESSAGE_DIFFERENCE, 2 EXPECTED_NATIVE_UNSUPPORTED, 3 REFERENCE_ONLY,
0 MISMATCH; `frame_check` confirms Bazel's `Error in <function>:` line of every
conflict occurs in native stderr.

## Loading (instantiation)

Implemented in `src/custom_rule.rs` (`implicit_outputs`,
`implicit_output_values`, `implicit_outputs_callback`) and
`src/implicit_outputs.rs`, after attribute defaults and computed defaults:

* **Templates** (`ImplicitOutputsFunction.java:416-506`): `%{x}` up to the
  next `}` is a placeholder; an unterminated `%{` is literal. Every
  combination of the placeholders' values is substituted (cartesian product,
  de-duplicated). Values (`attributeValues`, `:356-399`): `%{dirname}`
  (`sub/` or empty) and `%{basename}` of the rule name first; `%{name}`;
  string and string-list attributes as is; label and label-list attributes
  by label name without its last extension (`dir/in.tar.gz` -> `dir/in.tar`,
  `FileSystemUtils.removeExtension`); output and output-list names. Built-in
  attributes by their native type (`tags` is the effective tag list;
  `visibility` is `list(label)` NODEP and fails). Starlark private `_x`
  attributes are not addressable (their internal name is `$x`). An unknown
  attribute or an empty list gives no value.
* **Dict form** (`StarlarkImplicitOutputsFunctionWithMap`, `:133-161`): a
  configurable placeholder (a retained, non-trivial `select()`) fails first
  with `Attribute X is configurable and cannot be used in outputs`
  (`:509-525`); an unconditional `select()` is simplified at loading and is
  not configurable. No value: `For attribute 'K' in outputs: Invalid
  placeholder(s) in template`; other types: `For attribute 'X' in outputs:
  Attributes of type T cannot be used in an outputs substitution template`
  (T is `Type.toString`: `int`, `boolean`, `dict(string, string)`,
  `list(label)`, ...).
* **Function form** (`StarlarkImplicitOutputsFunctionWithCallback`,
  `:77-130`): the function is called with one positional argument per
  parameter name (a keyword-only parameter is therefore never bound and the
  call fails), on Bazel's transient thread without a package context
  (StarlarkCallbackHelper.java:64-77): package functions (`native.glob`,
  `native.package_name`, rule calls, ...) fail inside it. Bazel's texts name
  the function (`glob() can only be used while evaluating a BUILD file or a
  legacy macro`, `package_name() can only be used while evaluating a BUILD
  file or a legacy or symbolic macro`, `_f() does not accept positional
  arguments, but got 1`); bazel-rs reports a generic package-context text and
  starlark-rust's binding error after the same `In rule L:` prefix
  (EXPECTED_MESSAGE_DIFFERENCE rows). The native `.bzl` `native` struct has
  no `existing_rules` (pre-existing), so that call fails earlier. Arguments come
  from every nonconfigurable attribute by Starlark name (private `_x`
  included; unset labels are `None`; labels are `Label`s). A missing or
  `select()`-valued one: `Attribute 'X' either doesn't exist or uses a
  select() (i.e. could have multiple values)`. The result must be
  `dict<string, string>` (`got list for 'implicit outputs function return
  value', want dict`, `got dict<string, int> for ...`); each value is a
  template (no configurable check), more than one expansion fails with
  `expected one element but was: <a, b>`. A `fail()` reports `Error in fail:
  MSG` (Bazel prints the Starlark traceback first; frames are not compared).
* All of the above are prefixed `In rule //pkg:name: ` (Rule.java:575-577).
  Then each output name is checked like `Label.create`: `illegal output file
  name 'N' in rule //pkg:name due to: invalid target name 'N': ...`
  (Rule.java:536-551), and an implicit key equal to an output attribute with
  a value fails with `Implicit output key 'K' collides with output attribute
  name` (Rule.java:577-586).
* Implicit outputs become output file targets of the package, **before** the
  explicit `attr.output`/`attr.output_list` ones (Rule.java:526-528): that is
  their order among the rule's outputs and in the default files to build.
  `bazel-rs build //pkg:name.ext` and `query //pkg:name.ext` resolve them.

Package conflicts (`src/package.rs` `Recorder`, for every rule, not only
implicit outputs) use TargetRecorder's texts (`TargetRecorder.java:540-600,
693-715, 785-805`), checked at the declaring call like Bazel
(RuleFactory.java:325-326, StarlarkRuleClassFunctions.java:1896-1897,
StarlarkNativeModule.java:609-610) with its `Error in <function>:` frame, so
evaluation stops there (later errors, `fail()`s and declarations never
happen; `existing_rules` never sees the conflicting target): `rule 'x' has more than one
generated file named 'y'`, `generated file 'y' in rule 'x' conflicts with
existing (filegroup rule|generated file from rule 'g'|source file|package
group), defined at LOC`, `KIND rule 'y' conflicts with existing ...`,
`rule 'x' has conflicting output files 'a/b' and 'a'`, `output file 'a' of
rule 'y' conflicts with output file 'a/b' of rule 'x'`, `rule 'x' has file
'f' as both an input and an output` (inputs are Rule.getLabels: every
non-output, non-nodep label including `select()` keys and branches and
built-in attribute labels). `exports_files` creates source files
(TargetDefinitionContext.createInputFile, `:672-689`): idempotent for an
existing source file, else `source file 'x' conflicts with existing KIND,
defined at LOC`; locations are the call's parenthesis, like Bazel.

## Analysis

`ctx.outputs` (`src/analysis.rs`, StarlarkRuleContext.java:211-257) has the
output attributes, then one `File` per implicit key. Outputs.addOutput
(`:363-371`) rejects a key already present — also an *unset* output attribute
of the same name — and `executable` on an executable or test rule:
`in R rule //pkg:x: Multiple outputs with the same key: K`. Implicit outputs
without a generating action fail like any predeclared output (`The following
files have no generating action`).

rules_cc 0.2.17 `cc_binary`/`cc_test` declare `outputs = {"dwp_file":
"%{name}.dwp", "stripped_binary": "%{name}.stripped"}`
(`cc/private/rules_impl/cc_binary.bzl:899-902`, `cc_test.bzl:154-158`) and
read `ctx.outputs.stripped_binary`/`ctx.outputs.dwp_file`
(`cc_binary.bzl:714-716`); the fixture's `cc_like` rule mirrors that shape.

## Explicit native failures (divergences)

* Genuine Bazel 9.3 **crashes** (FATAL, exit 37) on: a dict-form template
  expanding to more than one file (`IllegalArgumentException`, uncaught in
  WithMap, e.g. `%{srcs}` with two labels or `%{tags}` with two tags), an
  unset label placeholder, and an unset `deprecation` (NullPointerException).
  bazel-rs fails explicitly naming the crash.
* An output named like its own rule: Bazel warns (`target 'x' is both a rule
  and a file; please choose another name for the rule`, Rule.java:638-644)
  and the generated file shadows the rule; bazel-rs fails explicitly.
* Output names with characters outside the native label subset (e.g. the
  literal `%{` of an unterminated placeholder) fail as before.
* Unset built-in string attributes other than via explicit values, and
  placeholders reading values the native profile resolves only later, fail
  explicitly. `--incompatible_no_rule_outputs_param` is not modelled.
* `query --output=label_kind` and `//pkg:*` patterns are not implemented
  natively (reference-only rows in the capture).
