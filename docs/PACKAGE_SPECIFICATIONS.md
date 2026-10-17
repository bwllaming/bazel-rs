# Package specifications: bounded implementation contract

This bounded native profile implements real group production, membership,
provider-constrained consumption and visibility. It is not full Bazel parity.

## Real producers and consumers

`package_group` declares an intrinsically public, zero-artifact target. A custom
rule consumes its native provider through a provider-constrained label attribute:

```starlark
def _impl(ctx):
    allowed = ctx.attr.group[PackageSpecificationInfo].contains("//app:target")
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, str(allowed))
    return [DefaultInfo(files = depset([out]))]

check = rule(
    implementation = _impl,
    attrs = {"group": attr.label(providers = [PackageSpecificationInfo])},
)
```

The provider constructor is available in BUILD- and MODULE-loaded `.bzl` files,
not directly in BUILD or MODULE files, and is not callable. Group targets also
have synthesized `DefaultInfo` with empty files.
The broader `DefaultInfo` runfiles and `files_to_run` APIs remain outside the
existing native profile; no placeholder values are added for them.
`contains` takes exactly one
positional Label or absolute label string. Strings use canonical repository
names, not apparent-repository mappings; `//` denotes the main repository.
Typed Labels retain their already-resolved repository identity.

## Membership and visibility

Specifications support exact packages, recursive subpackages, repository-qualified
packages, package/subpackage sentinel labels, `public`, `private`, and exclusions.
Each group's membership is its positive matches minus its negative matches,
**unioned with included groups**. An exclusion does not subtract an included
group's contribution. Includes must resolve to actual package-group targets.

Package and label literals use Bazel's full label grammar (N38,
docs/LABEL_SYNTAX.md): `//space here`, `//foo<bar` and `//(group)/...` are valid
packages, non-ASCII package names are invalid with Bazel's text, and
`PackageSpecificationInfo.contains("@repo")` parses `@@repo//:repo`
(`Label.parseCanonical`).

Includes are metadata dependencies, never source-file declarations or action
inputs. An owned, canonical, shared graph preserves group boundaries without
flattening every inclusion path. Construction bounds graph allocations and path
height, including cached subgraphs. Membership does not use Starlark instance
equality to deduplicate data: Bazel's native instances can compare equal despite
having different membership contents.

Visibility groups are resolved for every analyzed producer, including explicit
roots and eligible source targets. Same-package access does not bypass validation
of missing, wrong-kind, or cyclic group references. Query remains load-only.
Dependency authorization retains symbolic macro definition-package authorities;
source producers remain physically package-owned.

Symbolic macros may declare package groups. Such groups retain macro ownership,
name reservations and analysis-time namespace checks, but are intrinsically
public rather than macro-private. They are not ordinary rules: rules-only `:all`
and recursive selection exclude them. This increment does not expand the query
language or add `:*` selection.

## Required providers

Label and label-list attributes accept exported provider constructors. A flat
sequence is an AND clause; nested sequences are OR alternatives. Lists and tuples
are accepted. Every element is validated before normalization: an empty outer
sequence or any empty inner clause makes the predicate unrestricted. Strings,
instances, `None`, and unexported constructors are rejected.

Ordinary rule prerequisites must supply the required provider instances;
`DefaultInfo` is synthesized. Allowed source files skip this predicate, while
retaining file-admissibility checks. Actual package groups also skip ordinary
predicate matching and instead have a separate placement check. Bazel 9.3 uses
the normalized description's `PackageSpecificationInfo` or
`PackageSpecificationProvider` substring, including matching custom provider
names, plus its special attribute-name exceptions. This is not an exact-key or
provider-satisfaction check. Empty-clause normalization removes that exception.

Direct or nested custom-rule returns of `PackageSpecificationInfo` are explicitly
unsupported in this native profile. The reference crashes on the direct-return
case; native code must reject it with a controlled error, not reproduce the crash
or claim successful forwarding.

## Evidence and remaining work

Reference contracts and cleanup receipts are recorded in
`artifacts/package-specification-contract.json` and
`artifacts/package-specification-constraints-contract.json`. Some exploratory
attempts are explicitly marked superseded; their count is not a passing-test count.
The frozen native/reference matrix passes nine families, 154 phases and 290 CLI
commands in `artifacts/package-specification-validation.json`. The updated macro
regression passes eight families, 109 phases and 198 commands in
`artifacts/package-specification-macro-regression.json`. Both retain source,
registry and executable bindings, successful shutdown and scratch cleanup.

The preserved application probe advances past `PackageSpecificationInfo` and
stops at missing `RunEnvironmentInfo` in generated globals code. See
`artifacts/mainrepo-package-specification-loading.json`; the checkout and required
Git symlink remain unchanged. Seven adjacent suites pass on the same candidate;
the pinned analyzer reports no errors. The complete gate and manifest are
`artifacts/package-specification-check.log` and
`artifacts/package-specification-verification-manifest.json`.
The final gate passes 897 Rust and 490 Python tests, including 25 real package
specification CLI tests, 20 new harness tests and 24 updated macro-harness tests.

Full Bazel compatibility, application build/test success and performance goals
remain unfinished.
