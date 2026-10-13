# Root MODULE recording validation

CLI query/build now validate root MODULE before target loading. This is recording
validation, not full graph-resolution parity.

The CLI evaluates an existing root `MODULE.bazel` before
`query` or `build` loads BUILD files, extension definitions or actions. This uses
the existing full recording evaluator—not a symbol scan or an extraction of only
recognized directives. Invalid expressions must fail even when the selected
target is entirely in the main repository.

## Recording is not dependency resolution

Preflight records declarations with the actual root role, ignore-development
setting and built-in profile. It does not resolve unused dependencies, read their
registries, load their definitions or execute module-extension/repository
callbacks. Deferred execution-profile errors remain deferred until demand.

Low-level repository, label and package APIs retain their existing lazy behavior.
The new preflight belongs at the CLI boundary. This does not establish complete
Bazel graph-validation or eager-resolution parity.

## One root snapshot

A repository runtime retains the canonical workspace directory and either the
recorded root module or its original missing-file diagnostic. Later graph demand
reuses that snapshot: the root is not read or evaluated a second time. MODULE
prints and computations therefore do not repeat merely because an external label
is subsequently resolved.

Cloned handles share success, failure and absence. Changes to MODULE after the
first recording do not alter that runtime's snapshot; a fresh runtime observes
them. In particular, creating a previously missing MODULE does not make an
already-recorded missing snapshot silently change during graph demand.

Only `NotFound` from reading MODULE represents optional absence. Other read,
directory, UTF-8 and evaluation errors remain errors. Existing non-MODULE
workspace markers continue to support local commands. Workspace discovery,
startup option and rc handling are unchanged; `mod graph` retains its existing
evaluation path.

## Retained failure and closure evidence

The prior metadata increment retained a genuine mismatch: a local native query
accepted a root MODULE call to undefined `RunEnvironmentInfo`, while Bazel 9.3
rejected it. That was a loading-policy gap, not a provider installed in the wrong
globals. The original failed report and exact tracked excerpt remain unchanged:

- `artifacts/run-environment-info-validation-first-attempt.json`
- `tests/fixtures/run_environment_info/module_gap.json`

New evidence in `artifacts/g-root-module-validation.json` passes four families,
29 phases and 59 commands. The exact original local-query case now rejects with
native exit 1/reference exit 48, empty stdout, no loading/callback markers and no
action stores. `gap_closure.native_closure_verified` is true. The updated metadata
matrix passes seven families, 98 phases and 186 commands, retaining both local
and external-demand rejection checks.

Fifteen new CLI tests cover early errors, valid output, single evaluation on later
external demand, intentional laziness, filesystem errors and command ordering.
The full gate passes 952 Rust and 533 Python tests; strict Clippy, formatting,
release build and pinned analyzer diagnostics pass. Ten adjacent suites pass on
the same frozen binary. Source/report bindings are recorded in
`artifacts/g-root-module-verification-manifest.json`.

The preserved application query still stops at missing `set`, as recorded in
`artifacts/mainrepo-root-module-loading.json`. Checkout contents and the required
Git symlink are unchanged, and owned scratch is removed.

Full Bazel compatibility, application build/test success and performance goals
remain unfinished.
