# Bounded symbolic macros

This profile implements real macro expansion and declaration ownership, not a
loading-only `macro` placeholder: user-defined attributes, attribute
inheritance (`inherit_attrs`) and nested symbolic macros are supported;
finalizers are not (an invocation fails explicitly).

## Construction and execution

The constructor is available in BUILD-loaded and MODULE-loaded `.bzl` files,
and requires `.bzl` initialization. It is not installed in BUILD or MODULE files
themselves. Implementations must be Starlark functions; private helpers and
closures are supported, native builtins and callable descriptors are not.

The schema is `name`, `visibility`, the `attrs` entries in dict order (an
entry whose value is `None` is dropped; `name`/`visibility` are rejected with
genuine's `Cannot declare a macro attribute named '...'`; computed and
late-bound defaults are rejected), then the inherited attributes
(StarlarkRuleClassFunctions.macro, :414-505). `inherit_attrs` takes an
exported Starlark rule (its built-in attributes for its kind — test,
executable — then its public schema attributes in rule-class order), a native
rule function (`native.filegroup`, the registered classes of
docs/NATIVE_BUILD_RULES.md), an exported macro (its attributes, including
`_x` ones), or `"common"` (the Starlark base rule's attributes). Inherited
attributes skip `name`, `visibility`, undocumented `generator_*`, private
attributes and every key of `attrs`; an optional inherited attribute's
default is `None`. With `inherit_attrs` the implementation needs `**kwargs`.
Finalizers load (documented by `starlark_doc_extract`,
docs/STARLARK_DOC_EXTRACT.md) but an invocation fails explicitly (`macro
finalizer=True is unsupported`).

The first global assignment establishes export identity, including a private
underscore-prefixed assignment. Aliases and reexports preserve it. A descriptor
only held in a container is not exported. When an imported factory constructs a
macro, its definition package belongs to the initializing `.bzl` file, not the
factory implementation's lexical package.

Invocation takes keyword arguments only (MacroClass.instantiateMacro,
:178-280): an unknown one fails (`no such attribute 'x' in 'm' macro`, with
did-you-mean), `None` means unset, a `_x` attribute cannot be set. The
implementation receives `name`, `visibility` and every attribute, in schema
order. A given value is converted like Bazel's populated attribute:
type-checked (`expected value of type 'int' for attribute 'e' of 'm', ...`),
label strings resolved against the package into Labels (outputs too),
order-independent lists (`tags`, `features`, ...) sorted, unconditional
`select()`s simplified; a remaining `select()` keeps its elements with Label
keys (a plain element becomes a `//conditions:default` branch). An unset
attribute gets its default (`None` for inherited ones and null labels). A
configurable attribute's non-`None` plain value reaches the implementation
as `select({"//conditions:default": value})`. Conversion errors of
Starlark-defined attributes stop evaluation at the call; missing mandatory
attributes and conversion errors of built-in/native attributes are package
errors (`//pkg:x: missing value for mandatory attribute 'mand' in 'm' macro`)
reported after the BUILD file finishes, and the implementation still runs
with the attribute unset. The implementation must return `None` (`macro 'x'
may not return a non-None value (got 7)`). The name is a required string.
Unlike the pinned reference's crash on `name=None`, the native implementation
rejects that malformed invocation with a controlled error; this is an
intentional robustness difference.

The explicitly given dependency-label values of a top-level macro (not of a
nested one) are assumed input files of the package, like an ordinary rule's
(Package.createAssumedInputFiles, packages/Package.java:893-925).

A macro may call other symbolic macros. Inside a macro an omitted
`visibility` is private, and the grant added is the enclosing macro's
definition package. A submacro's name must be within the enclosing macro's
namespace (`macro 'x' cannot declare submacro named 'other'. Name must be the
same as ...`); the first submacro may reuse the enclosing name, a second may
not (`macro 'x' conflicts with an existing macro (and was not created by
it)`). Generator attributes name the outermost macro's BUILD call. A macro
instantiated (transitively) by an instance of its own macro class is a
package error (`macro 'x_a' is a direct recursive call of 'x'. Macro
instantiation traceback ...`) and does not run.

## Visibility and source existence

Invocation visibility uses the explicit value or the package default at the time
of the call. `None` means default; an empty list means private. The caller package
is added as an exact grant unless already present. Public visibility remains
public; private tokens are removed when other grants exist. The callback receives
a **frozen list of canonical Labels**, sorted by repository, package and name.
Repeated nonprivate labels remain repeated, and an ancestor's recursive grant
does not suppress the caller's exact grant. Caller-owned lists are not frozen.

Macro-generated rules with omitted visibility are private regardless of BUILD
package defaults. Their effective visibility includes the macro definition
package. Dependency access uses the producer and consumer definition packages
for macro-owned rules, not their physical BUILD packages. Thus a caller-package
rule cannot gain access to a macro-private target solely by sharing its physical
package. Generated outputs inherit their generating rule's ownership.

Existence is checked separately from access. A macro-generated rule cannot create
an implicit source target merely by referencing a physical file (a top-level
macro's own label-valued attributes can, see above). Sources must be
explicitly exported outside the macro or established by an ordinary declaration's
supported dependency-label attributes, including defaults. Ordinary declarations
need not be selected for analysis and may appear before or after the macro call.
Canonical repository and package identities must match. Reserved macro names
cannot become implicit input targets.

Later `package(default_visibility=...)` calls preserve the existing ordinary-rule
behavior but do not retroactively change a macro's earlier visibility snapshot.
Package groups were outside the original macro increment's visibility profile.
The subsequent [package-specification increment](PACKAGE_SPECIFICATIONS.md)
adds group resolution; broader configured visibility remains unfinished.

## Names and phase restrictions

Each invocation reserves its exact name even if it declares no targets. Existing
rules, predeclared outputs, exported sources and other macros cannot share that
name (genuine's `macro 'x' conflicts with an existing target.` and `target 'x'
conflicts with an existing macro (and was not created by it)`). The invocation may declare its own same-name target. Overlapping prefixes
alone do not reserve one another's names or confer ownership.

A macro-owned target name must equal the invocation name or start with that name
followed by `_`, `-` or `.` and a nonempty suffix. Violations remain query-visible
but fail when analyzed. An otherwise valid rule may produce an out-of-namespace
output; explicitly building that output fails. A valid output name cannot bypass
an invalid generating rule. Checks precede analysis memoization, so root order
cannot hide an invalid requested output. Dynamic action artifact filenames are
not independently declared package targets and are not subject to this check.

`glob()` and `package()` are forbidden inside ordinary symbolic macros, including
through legacy helpers. `native.package_name()` continues to identify the physical
caller package. In-macro `exports_files()` is explicitly unsupported by this
profile, although Bazel allows it. Ordinary legacy helper behavior is unchanged.

## Evidence and limits

`tests/symbolic_macro_attrs.rs` replays the genuine 9.3.1 capture
`tests/fixtures/symbolic_macro_attrs/reference_receipt.json` (34 cases: the
kwargs of every inheritance source, select/label conversion, nested-macro
visibility and naming, every macro()/invocation error text, `query
--output=build` of macro-created targets, and a build through an
`inherit_attrs` macro modeled on toolchains_llvm's `resource_directory`).
Declared divergence: the `# Rule x instantiated at` call-stack comment of
`query --output=build` shows the macro callables in the native traceback
layout (`<macro m>`), not Bazel's `m`/implementation frames.

Pinned reference constructor/phase probes are under
`/tmp/opencode/next-d-macro-contract/`; output, collision, visibility and implicit
source probes are under `/tmp/opencode/symbolic-macro-edge-contract/`.
The original macro increment's native/reference matrix passed eight families and
108 phases, with 191 actual
CLI commands, in `artifacts/symbolic-macros-validation.json`. It records frozen
source/binary bindings, reference shutdown and scratch cleanup. That increment's
final gate passed 838 Rust and 469 Python tests, including 17 real CLI macro tests
and 23 harness tests. Seven adjacent suites passed on that candidate.

At that increment, the preserved application query advanced past
the `macro` global and stops at missing `PackageSpecificationInfo` in generated
globals code. See `artifacts/mainrepo-symbolic-macros-loading.json`; checkout,
required symlink contents and scratch cleanup remain verified.
The later package-specification probe advances to missing `RunEnvironmentInfo`.
Full application builds/tests, complete macro compatibility, SDKs, configured
analysis and performance parity remain unproven.
