# Reference provenance

Independent foreground captures (2026-10-28):
`/home/coder/opencodetmp/target-compatibility-reference/REPORT.md`, GRAPH.tsv,
`captures/*/receipt.json` and raw stdout.pb/stdout.textproto.

- Genuine Bazel 9.3 ELF SHA256:
  `d302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8`.
- Pinned source: `e89755babf3e941bc1e9d8120b1cd48854ab9dfa`.
- Capture 06 binary analysis.CqueryResult SHA256:
  `55655a3fd201fcd0d14e1b39e4799405a467e8c628d9d17ce93ee611405bdf94`.
- 12 invocations, plus four separately retained setup failures. No completed
  genuine captures were rerun by the implementation worker.

Captures 01/06 establish resolved suppressed-dependency attributes and exact
configured edges; 02–04 establish label success vs invalid-select textproto
crashes; 05 establishes compatible missing-package/provider/visibility/testonly
controls; 07–09/11 establish base nonapplication versus aspect-own empty views
and provider validation; 12 establishes alias/label_flag/label_setting provider
identity. Capture 10 is setup-blocked (missing offline rules_swift source), not
evidence of an aspect-own toolchain result.

The Rust tests use small local constraint/platform equivalents of these source
patterns. Selected/inactive deps appear in declaration-level rule_input, while
only analyzed config conditions and incompatible constraints are direct edges.
An indirect consumer retains actual ordinary prerequisites and its platform.
The output's generating_rule string alone does not establish a graph edge.

An additional native replay uses the exact captured focus/hidden/platforms source
copied into private scratch. Both formats are decoded using google.protobuf and
the checked-in official `cquery_proto/schema.pb`; receipt:
`/home/coder/opencodetmp/target-compatibility/reference-replay-receipt.json`.
Label/kind sets, label-edge sets and selected attrs match. Native has 29 rather
than 31 configured results because its pre-existing model lacks separate
untrimmed top-level flag/setting identities (docs/TARGET_COMPATIBILITY.md).
Tests use dump-relative IDs/checksums, never genuine checksum constants.
