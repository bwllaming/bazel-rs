# Extension-context native introspection

MODULE-loaded `.bzl` native bindings expose `existing_rule(name, /)` and
`existing_rules()`. Their supported behavior is determined by the **active
evaluator context**, not merely by where a function was loaded.

During an ordinary module-extension callback:

- `existing_rule` accepts exactly one positional string and returns `None`.
- `existing_rules` accepts no arguments and returns a frozen, empty dictionary.
- These results remain empty after repositories have been recorded. Strings are
  not validated as repository names, and recorded repositories are not queried.
- Mutating the returned dictionary fails, including `clear()` when already empty.
  An explicit `dict(native.existing_rules())` creates a writable copy.

Calls during `.bzl` initialization or repository callbacks reject with Bazel's
phase diagnostic. Aliased functions retain this dynamic check; binding an alias
during initialization does not authorize a later repository callback. A failed
callback does not publish partial repository files.

This follows Bazel 9.3's extension-evaluation no-op marker. It is **not** an
unconditional MODULE stub. In particular, genuine `maybe()` still invokes a rule
for an already-recorded name; the existing duplicate-declaration guard must reject
the second invocation rather than silently skip it.

## BUILD and legacy-macro introspection

During BUILD evaluation (the BUILD file's top level, `native.` in a
legacy macro) both functions read the package being evaluated
(StarlarkNativeModule.java:320-521, src/existing_rules.rs). Fixture:
`tests/fixtures/existing_rules_build` (genuine 9.3.1 capture,
`tests/existing_rules_build.rs`).

- `existing_rules()` returns an immutable view (`type()` is `Map`, repr
  `<native.ExistingRulesView object>`) over a **snapshot**: the rule targets
  declared so far, in declaration order. Rules declared later are not in an
  earlier view. Package groups, files and outputs are not rules.
- `existing_rule(name)` returns `None` or an `ExistingRuleView` (repr
  `<native.ExistingRuleView for target 'NAME'>`). Its keys are `name`,
  `kind` (the rule class: a Starlark rule's exported name) and each
  exportable attribute in rule-class order: names starting with a letter,
  with a value Starlark can represent. Computed defaults (`testonly` unless
  the class fixes it, `deprecation`, `package_metadata`, a test's
  `timeout`), licenses and unset label/output attributes are absent.
- Values follow `starlarkifyValue`: lists are tuples, dictionaries are
  immutable, labels are relative to the package (`:x`, `//pkg:x`,
  `@@repo//pkg:x`) and order-independent lists (`tags`, `features`,
  `visibility`) are sorted. A `select()` value is a select whose elements
  are all selects keyed by `Label` values (a plain element under
  `//conditions:default`).
- Views support `[key]` (`key "k" not found in view`), `in`, `len`,
  iteration over keys, `get(key, default=None)`, `keys()`/`values()`/`items()`
  (frozen lists), `dict(view)` and `dict.update(view)` (a vendored-Starlark
  `Map` hook), and identity equality and hashing. Assignment fails with
  `can only assign an element in a dictionary or a list, not in a 'Map'`.
- Inside a symbolic macro (also through a legacy macro it calls), at `.bzl`
  top level and in transient callbacks (an implicit-outputs function), both
  fail with `existing_rules() can only be used while evaluating a BUILD file,
  a legacy macro, or a rule finalizer`. Finalizers are not supported.
- `--incompatible_existing_rules_immutable_view` is a no-op in 9.3.1, as in
  Bazel.

The views only read the declaring package's own evaluation state, so a
package's result still depends only on its BUILD file and loads; the
analysis snapshot and incremental tracker need no new dependency.

`artifacts/native-existing-rules-contract.json` contains 30 genuine reference
probes and pinned source evidence for signatures, phase checks, mutation rejection
and the real `maybe()` duplicate behavior. The completed native/reference matrix
passes six families and 70 phases, including 90 actual CLI commands, in
`artifacts/existing-rules-validation.json`. It executes the untouched upstream
`bazel_features` extension through the genuine skylib wrapper and `utils.maybe`,
then compares all four generated files on cold/warm runs. Source/binary stability
and reference shutdown/scratch cleanup are recorded.

The application at the introspection increment loaded the generated globals file and failed at the
missing `macro` global; see `artifacts/mainrepo-existing-rules-loading.json`.
Fixture version 1.42.1 does not establish the application's selected version.
The subsequent [symbolic-macro increment](SYMBOLIC_MACROS.md) advances that probe
to missing `PackageSpecificationInfo`, without claiming complete features-library
execution.
Full application build/test, complete
features-library loading, SDKs and performance parity remain unproven.
