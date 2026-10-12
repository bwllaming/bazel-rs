# Explicit-module graph fixtures

`cases.json` describes private, collision-resistant module names and hand-written
expected trees. `scripts/validate_modules.py` generates registry `MODULE.bazel`,
metadata and `source.json` files in a fresh private copy. Every source is an
absolute `local_path`; no checkout files are mutated.

The official reference additionally uses the real Bazel Central Registry for
its genuine implicit built-in closure, including extension evaluation. Nothing
overrides or replaces `bazel_tools`. Both commands use `--noinclude_builtin`,
which hides the built-in subtree, not arbitrary extra root dependencies. Parsed
JSON is compared without filtering, sorting arrays, or deleting fields.

Canonical repository names are not exposed by this JSON profile; multiple-version
canonical multiplicity and nodep repository mappings require resolver unit tests.
Native extension/remote-registry rejection is a capability boundary, never an
official parity failure. This suite is not a full Bzlmod compatibility claim.
