# Scoped repository fixture

Run `python3 scripts/validate_repositories.py` from the checkout. Defaults are
the native release binary, official Bazel 9.3.0 ELF binary, a 90-second command
timeout, and `artifacts/repository-validation.json`. Override them with
`--native`, `--reference`, `--timeout`, and `--output`.

The harness copies this tree into private scratch space. It never builds an
arbitrary user repository or invokes Bazel `run`, `test`, or publication. Shell
actions are only the fixture's trusted `cat` commands; metadata uses
`ctx.actions.write`. Native actions are unsandboxed.

## Coverage

- Root aliases `first` and `second` select root-relative overrides. Alpha sees
  beta as `dep`, not `second`; the root cannot use alpha's `dep` alias.
- Main, alpha, and beta all have `p/data.txt` and generate `p/copy.txt` with
  different bytes. Native counts must be `(3 executed, 0 hits)`, then `(0, 3)`,
  then `(1, 2)` after changing only alpha's private-copy source.
- Alpha's provider retains an external source File absent from DefaultInfo,
  forwards it, and supplies two-action consumers in main and alpha. Each cold
  build must execute two actions and each warm build must have two hits.
- `Label("//p:data.txt")` is definition-relative to alpha. Metadata compares
  exact canonical owners, ctx labels, source exec paths and short paths.
  Generated exec paths are deliberately not embedded in metadata.
- `declare_file` and its sibling output retain the correct external repository
  directory. Private visibility and external `__pkg__` cannot authorize the
  same package spelling in main; `@@//p:__pkg__` can. External `@//` is rejected;
  external `@@//` addresses main.
- A second private copy replaces the root MODULE with a registry dependency.
  A private file registry's `source.json` selects alpha's absolute live source
  directory. The registry MODULE omits beta even though alpha's source MODULE
  declares it. Copy builds prove source selection and live invalidation with
  native counts `(1, 0)`, `(0, 1)`, `(1, 0)`; rejecting alpha's `@dep` proves the
  registry MODULE, not the source MODULE, controls the dependency mapping.

## Evidence and limitations

Official query prints root apparent aliases such as `@first//p:copy`; native
prints canonical labels such as `@@fixture_alpha+//p:copy`. The report retains
both exact label lists, reports unequal spellings, and compares identity only
through the fixture's explicit alias map. This is not exact query-text parity.

Artifact keys retain repository/package identity and normalize only the
explicit `bazel-out/{rust-fastbuild,k8-fastbuild}/bin` prefixes. Bytes, hashes,
sizes, and output cardinalities must match. Modes are captured and differences
reported, not claimed equal: official Bazel commonly makes outputs `0555`.

Official servers, output bases, repository download caches, and content caches
are isolated and shut down before scratch removal. The registry case adds BCR
only to official Bazel for its genuine built-in dependencies; native sees only
the local registry. There are no fake tool repositories or old native caches.
Official internal action counts are not equated to native counts, and jobs=4
does not itself establish concurrency. This is scoped parity, not complete
Bazel compatibility.

`reference_fixture_checks(reference, scratch)` and
`registry_case(None, executable, scratch, runner)` provide official-only fixture
probes for integration work. Their results explicitly do not establish native
parity. The caller must shut down its Reference and remove its scratch tree;
`registry_case` shuts down its own Reference in `finally`.
