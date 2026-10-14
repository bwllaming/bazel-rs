# Identity-preserving Label construction

`Label(input, /)` accepts either a string or an existing native Label. Existing
Labels are returned unchanged, before consulting the caller's repository mapping
or parsing a string. Passing an external Label through a root helper, or a root
Label through an external helper, does not remap its canonical identity.

String arguments retain the existing lexical resolution and parsing behavior.
A typed union keeps wrong-type rejection in the builtin argument binder; there
is no string coercion or acceptance of objects that merely resemble a Label.

The argument is mandatory and positional-only, matching Bazel 9.3's `input`
signature. This intentionally tightens the former native `label_string` keyword
acceptance: both `Label(input=...)` and `Label(label_string=...)` are rejected,
as are missing/extra arguments and other unsupported value types.

## Ownership and identity

The passthrough branch returns the original Starlark Value. It does not allocate
a replacement Label, redo repository resolution, or change hashing, equality,
canonical strings or cache formats. Unit tests check pointer identity, stable
heap usage across repeated calls, dictionary keys and frozen inputs/helpers
after their original owners are dropped. CLI fixtures exercise conflicting
owner mappings, generated repositories, frozen exports and actual analysis
File/Target/output owners, with consumed output bytes and cold/warm cache checks.

## Limits

Native Label construction still rejects unknown apparent repository names eagerly.
Genuine Bazel can instead construct a non-visible Label, pass that value through
`Label(...)`, and reject a later visibility-sensitive getter. This increment
does not add those invalid states or claim equivalent failure timing.

Other Label members and deprecated-API flags are separate work. Passthrough tests
use existing canonical strings and `workspace_name`, without depending on the
independent `repo_name` increment. No full Label, application or Bazel parity is
implied. Completed action outputs retain the existing `0555` policy; repository
source files have their separate permissions, and callback repetition is not
used as a proxy for publication or action-cache reuse.

The bounded comparison and its replay checks are defined in
`scripts/validate_label_passthrough.py`; reference preparation, native execution
and final acceptance are separate receipts.
