# Local repositories: implementation and validation boundaries

This increment extends native loading to existing local external repositories.
It is not full Bzlmod, Go-rule, application build/test or performance parity.
No command delegates execution to official Bazel.

## Implemented scope

- `local_path_override` provides a live local source root for external BUILD and
  `.bzl` loading. Canonical repository names identify packages/artifacts;
  apparent aliases resolve through the requesting module's repository mapping.
- `.bzl` `Label()` values and attribute defaults resolve in lexical definition
  context, not whichever BUILD file invokes a macro. Explicit caller strings
  retain caller context. Private/package/subpackage visibility includes repository
  identity; identical package paths in different repositories are not the same
  package.
- External source `File.path` uses `external/<canonical>/pkg/file`, while
  `File.short_path` uses `../<canonical>/pkg/file`. Short paths are metadata only,
  never source lookup or execution paths. Generated artifacts keep repository
  identity under the native output tree. Action-cache format remains
  `native-action-v4`; output paths need not match Bazel's configuration directory.
- Main-only source operations do not eagerly evaluate MODULE metadata. External
  alias/root resolution triggers the selected-module graph as needed. This lets
  unrelated source-only targets work without claiming their external application
  dependencies can be loaded.
- `query`, `build` and `mod graph` accept explicit filesystem registries. For
  example, from a workspace with the corresponding module metadata:

  ```sh
  bazel-rs --ignore_all_rc_files query --registry=file:///absolute/registry '@alias//pkg:target'
  bazel-rs --ignore_all_rc_files build --registry=file:///absolute/registry '@alias//pkg:target'
  bazel-rs --ignore_all_rc_files mod graph --output=json --registry=file:///absolute/registry
  ```

  The option is not a startup flag. Without it the default registry is
  `https://bcr.bazel.build/` (Bazel 9.3); pass an explicit `file://` registry to
  stay offline (docs/REGISTRY_LOCKFILE.md).
  Canonical `@@repo//pkg:target` labels bypass apparent-alias lookup, but loading
  still requires an available selected repository source root.

The library's legacy `Label::parse` deliberately remains main-repository-only.
Repository-aware CLI/loading paths use `RepositorySet::parse` and
`Label::parse_in`; a rejection from the legacy helper does not mean all external
CLI labels are unsupported. `@@//` explicitly names canonical main. In an
external repository, `@//` is an empty apparent-alias lookup, not shorthand for
main or self; an unavailable alias fails explicitly.

### Registry local-path source support

The current `src/repository.rs` implementation reads
`modules/<name>/<version>/source.json` from the selected filesystem registry and
accepts `{"type":"local_path","path":"..."}`. An absolute path names an existing
source directory. A relative path requires `module_base_path` in the registry's
`bazel_registry.json`; a relative base is resolved against that registry, not
the workspace. Registry MODULE metadata drives module selection; its directory
is not mistaken for a source checkout.

Source-root lookup is lazy. These roots and local overrides are live inputs, not
downloaded or copied repositories. Unsupported source fields/transformations,
missing metadata, invalid roots and unsupported source types fail rather than
being silently accepted. HTTP/Git/archive retrieval, patches, repository rules,
module extension execution/generated repositories, implicit built-in closure,
toolchains and configured application analysis remain unsupported.

The source-root implementation passed 13 focused repository tests and a real CLI
scenario covering live source changes and registry metadata authority. A separate
review approved this delta. Reference differential status is recorded separately
below; native tests are not a substitute for that comparison.

## Evidence captured

### Module graph: scoped PASS, not repository execution parity

The inspected `artifacts/module-validation.json` completed at
**2026-10-12 22:39:02 UTC** with **PASS**, no harness errors and cleaned scratch.
It contains **11 exact parsed-JSON reference comparisons**, including array
order, without projecting away graph differences:

- chain, diamond selection, and dependencies raised by a discarded version;
- shared-node breadth-first expansion and local-override declared version;
- fulfilled nodep upgrade without a graph edge and multiple-version snapping/
  aliases;
- development dependencies enabled or ignored (including ignored overrides);
- cycles shown and hidden.

The profile is `--output=json --noinclude_builtin --extension_info=hidden
--lockfile_mode=off`. Five additional **native-only** probes reject a missing
module, unknown option, unsupported registry scheme, unsupported output and pending
extension execution. They are not five negative differential parity cases.
Canonical mappings and repository source roots are not established by JSON tree
equivalence alone. Binary hashes and command streams are retained in the report;
a rerun may identify a different binary.

### Local external repositories: scoped differential PASS

`artifacts/repository-validation.json` records **20 passing checks**, no harness
errors and cleaned scratch. Coverage includes canonical query labels, three-repo
output/cache isolation, source-change invalidation, provider-carried external
sources, lexical labels, sibling outputs, visibility/alias failures, and a local
registry's source selection and authoritative MODULE metadata. The report keeps
exact commands, bytes/hashes and native/reference executable identities.

The initial differential exposed premature native dependency resolution: an
unused target mentioning an unavailable repository blocked a valid sibling.
Native declarations now validate syntax while dependency analysis resolves real
repository aliases. The regression tests both successful selection and failure
when the unavailable dependency itself is requested. Invalid path syntax still
fails loading. Review approved this correction.

`artifacts/external-path-reference.json` is an **official Bazel 9.3 contract
probe only**, explicitly not native parity. It records source/generated File
paths and ownership, lexical labels, repository-qualified visibility failures,
alias behavior and generated bytes, using private roots. Its recorded example
source path is `external/fixturefoo+/p/src.txt`, versus short path
`../fixturefoo+/p/src.txt`. Its success cannot substitute for a native/reference
repository differential run.

Earlier custom/loading receipts report 39/24 passing checks. Their reruns do not
automatically cover new repository source features. The coordinating session's
reported full-check snapshot is 243 unit plus 53 integration tests (**296 Rust**),
plus **124 Python** tests. These are
capture-time receipts, not permanently current totals or tests rerun by this
documentation edit. `CUSTOM_RULE_VALIDATION.md` retains its historical hashes
and timings rather than relabeling them as results for a newer binary.

`artifacts/rust-analyzer-diagnostics.txt` records a completed, error-free
`rust-analyzer diagnostics . --severity error` scan. Formatting, strict Clippy
and release compilation also pass; `artifacts/check-current.log`
retains the full check receipt.

### Main application: historical loading failure capture

This section records an earlier capture, not the current state. On the current
tree the native query of `//src/beyondwork.ai/utils/str:str` succeeds and a native
build stops at an analysis gap (currently while loading registered toolchains during
`go_library`'s toolchain resolution); see
the evidence note in `docs/ROADMAP.md`.


`artifacts/mainrepo-application-loading.json` records this explicit native query
from `/home/coder/code`:

```sh
/home/coder/bazel/target/release/bazel-rs --ignore_all_rc_files query //src/beyondwork.ai/utils/str:str
```

It exits **1**, with empty stdout, because `aspect_bazel_lib@2.22.5` was not found
in local registries `[]`; remote fetching was unsupported at that capture. The report sets
`application_compatible: false`. Even query/loading did not complete: no Go
compilation, build/test success or application parity follows from this probe.

The probe requested no build/test actions and did not invoke the repository
wrapper or source its `.envrc`. Its before/after Git state is identical and
`unchanged: true`: staged and unstaged diff byte counts are zero; three existing
untracked files remain. This establishes preservation over that probe, not a
claim that the checkout was pristine or that unrelated concurrent edits can be
attributed. No checkout changes were reverted.

## Reproduction and remaining gates

```sh
python3 scripts/validate_modules.py
# Separate, scoped repository differential:
python3 scripts/validate_repositories.py
```

These reports do not complete the application compatibility gates. Existing local roots
are trusted inputs; native actions are not sandboxed. Official oracle isolation,
disabled remote-write channels and source-state checks do not imply network
hermeticity. No timings here establish equivalent cold conditions, large-repo
scaling or a speed advantage.

Reviewed paths: the three evidence reports above, `src/repository.rs`,
`src/label.rs`, `src/main.rs`, `src/loading.rs`, `src/custom_rule.rs`,
`src/visibility.rs` and `src/engine.rs`. The module JSON gate, local repository
loading gate, registry source-root gate and full application gate are distinct.
