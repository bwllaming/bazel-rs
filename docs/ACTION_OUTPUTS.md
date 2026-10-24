# Regular action output permissions

Bazel 9.3 normalizes completed regular action outputs to `0555` when collecting
output metadata. This applies to genrules, shell/direct process actions and
`actions.write`, including writes with `is_executable=False`. The native engine
now performs this normalization after successful execution and validation,
before reading completed output bytes and publishing the complete output set.
This also permits successful actions that initially leave regular outputs at
mode `000`; path validation precedes chmod so symlinks are not followed.

Initial write permissions and shell-selected modes remain visible during the
action. `WriteAction.executable` still participates in the action key; distinct
write requests do not collapse merely because their published modes match.
Source and repository file permissions and manifest-file permissions are not
changed by this policy. Staged inputs are read-only hardlinks to the
content-addressed store (`0444` plus the keyed executable bits) since N36
(docs/HARDLINK_STAGING.md).

The `native-action-v5` cache identity invalidated earlier mode-preserving entries
(manifests were `native-action-v7` after N36, `native-action-v8` with the key
since the N41 x N42 merge, and are `native-action-v9` since fix-digest-memo;
docs/ACTIONS_PARAMS.md, docs/HARDLINK_STAGING.md, docs/REMOTE_CACHE.md,
docs/DIGEST_MEMO.md "Upgrade note").
Cache hits require exact `0555` output permissions, executable masks and content
digests. Writable, non-executable or privileged-mode tampering causes a rebuild.
The upstream writable-output option (`0755`) is not implemented. Tree
artifacts (docs/ACTIONS_PARAMS.md) normalize the root and every entry below
it to `0555`; symlink outputs are real symlinks and so show their target's
mode (docs/HARDLINK_STAGING.md).

The pinned reference contract is `OutputPermissions.READONLY` and
`ActionOutputMetadataStore.getOutputMetadata`: commit
`e89755babf3e941bc1e9d8120b1cd48854ab9dfa`. Six genuine local-execution reference probes,
including shell-observed modes before normalization, are retained in
`artifacts/action-output-mode-contract.json`. Native cold/warm regressions are
in `tests/output_permissions.rs`; the builtin helper-copy comparison remains
strict about both bytes and executable mode.
