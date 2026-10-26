# Configured aspect edge ownership (Bazel 9.3.0)

`nodes.txt` and `edges.txt` are independently decoded genuine results, not native
expectations inferred from reachability. Each final column lists the cases in
which the preceding tuple occurs:

0. `deps(set(//:requester //:twin //:plain //:via_alias //:via_decorated))`
1. Same with `--noinclude_aspects`
2. Same with `--noimplicit_deps`
3. Same with `--notool_deps`
4. `deps(//:branch) --aspects=//:defs.bzl%closure`
5. `deps(//:exec_requester)` (additional `exec-nodes.txt`/`exec-edges.txt`)
6. Same with `--notool_deps`
7. `deps(//:branch) --universe_scope=//:exec_requester --notool_deps`

All use `--define=branch=one --define=aspect=one --platforms=//:platform
--host_platform=//:platform --extra_toolchains=//:tc --output=proto
--transitions=lite`. The capture used the pinned Bazel 9.3.0 ELF and the actual
`analysis_v2.proto`/`build.proto` descriptor in `../cquery_proto/schema.pb`, with
`../cquery_proto/fixture.py`'s Google protobuf decoder. No remote downloads:
the genuine scratch MODULE uses the existing local platforms copy and builtin
closure stubs from `test_flags`. Native uses the explicit-module profile.

Normalization is only record/edge ordering, target/exec/null configuration
identity instead of engine-specific checksums, and platform configurations:
Bazel trims platforms to one empty-options configuration, while native retains
the incoming ConfigIds. No source/edge owner is normalized or rewritten.

The chain `closure requires middle requires base` propagates along `deps`.
`base` privately requests a source, a configured selector, an exec tool, an
exec-labelled source, and a toolchain. A separate attribute-attached aspect is
encountered on `decorated`. Two requesters and an unrelated plain consumer share
`branch`; the latter must not acquire any of these aspect-only inputs. An alias
also forwards the application. Top-level aspects are analyzed but do not create
forward edges from their base configured targets in non-explicit cquery.

Important upstream details pinned here:

* `PostAnalysisQueryEnvironment.targetifyValues` recursively flattens **each
  requester's** aspect dependencies and visits each aspect class once. It never
  attaches an aspect's private dependencies to its base rule.
* Base config conditions are direct CT deps, not prerequisites classified by
  `Util.findImplicitDeps`: they survive `--noimplicit_deps`. The same condition
  flattened from an aspect is implicit on the requester and is filtered out.
* `--notool_deps` tests endpoint configurations, not the attr transition. Thus an
  exec-labelled source survives on a target-config requester, while an exec
  configured rule does not. A toolchain implementation remains target-config.
  For an exec-config parent, only exec-config children remain: sources and the
  trimmed platform context are filtered out without rekeying native ConfigIds.
* Required and different-class propagated aspects must retain their provenance;
  unioning every applied aspect on a base overapproximates unrelated consumers.

Live proof receipts and capture driver:
`/tmp/opencode/cquery-aspect-edges-proof/` and
`/tmp/opencode/perf-research/cquery-aspect-edges-probe.py`.
The official TD v0.34 scratch harness, unchanged protocol/no result rewriting,
is specialized by `/tmp/opencode/perf-research/cquery-aspect-edges-td.py`.
Its nine cases pin unchanged, README, private witness, selected/unselected
aspect source, exec tool, toolchain source, aspect-only condition and ordinary
source changes. All aspect-only cases leave `plain`, `branch` and `chosen`
unaffected. Genuine results are in
`/tmp/opencode/cquery-aspect-edges-td-genuine/report.json`.
