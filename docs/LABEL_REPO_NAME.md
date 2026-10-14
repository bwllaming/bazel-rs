# Canonical repository names on resolved Labels

`Label.repo_name` returns the Label's stored canonical repository name, without
`@`. Main-repository Labels return the empty string, even when the root module
has a name or self-alias. The value matches the existing `workspace_name` getter;
`getattr`, `hasattr` and `dir` expose the new property. Labels remain immutable.

The accessor does not resolve names again in its reader's context. Apparent
aliases are resolved by their existing lexical or tag-owner mappings before
allocation. Generated repository names retain their complete canonical name.
An explicit canonical name does not require the referenced repository, package
or target to exist merely to read this metadata.

No stored identity, parser, hashing, equality, string formatting, repository
fingerprint or action-cache format changes. Frozen Labels and Label values from
File owners and Target labels use the same accessor. File/Target ownership and
owner-drop checks are unit-level evidence, not a claim of complete configured
analysis coverage.

## Important failure-phase limitation

Genuine Bazel 9.3 can construct a non-visible Label from an unmapped apparent
repository name. Reading its `repo_name` subsequently rejects that visibility
marker. Native construction instead rejects the unknown apparent name eagerly;
no invalid Label or later getter evaluation is fabricated. External `@//` has
the same existing native boundary, unlike explicit canonical main `@@//`.

This increment supports canonical-name access on already-resolved Labels. It
does **not** claim the deferred-invalid-Label behavior used by upstream callers
that discard the getter result to validate visibility. Tests record these
different failure phases separately rather than counting them as parity passes.

This increment also does not add `Label(existing_label)`, deprecated-API flag
handling, other missing Label members, BUILD-level Label/print globals, or a
`repo_name` field on extension participant-module objects. The wider reference
`dir(Label)` surface and direct-print formatting are not normalized into native
parity assertions.

## Verification scope

Focused tests cover main/self aliases, conflicting external-owner mappings,
captured tag defaults, generated canonical identities, nonexistent canonical
names, immutable/unknown properties, and eager rejection boundaries. Real native
extension output is consumed by cold/warm actions. Stable publication identity,
manifests, bytes and inventories are checked independently of callback messages:
native callbacks can repeat while consumer actions hit their cache.

The comparison harness uses a pinned genuine platforms archive and private
offline reference caches; registry metadata alone is not an archive. Completed
analysis output permissions are checked as `0555`, while repository source files
retain their separate `0644` requirement. Preparation failures and subsequent
frozen comparison receipts remain distinct. Portable tests do not require ignored
research artifacts. See `scripts/validate_label_repo_name.py` for the exact
bounded cases and replay checks; no full Label, application or Bazel parity is
implied by this accessor.
