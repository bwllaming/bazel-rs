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

## Explicit unsupported surface

Bazel also exposes these functions for real BUILD/legacy-macro rule introspection.
That separate view/snapshot behavior is not implemented here: native BUILD-loaded
`.bzl` bindings do not expose these fields. BUILD and MODULE files themselves do
not gain a `native` global. No empty-result substitute is provided for BUILD
introspection, configured analysis or rule finalizers.

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
