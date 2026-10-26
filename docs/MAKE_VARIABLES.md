# The `toolchains` attribute and Make variables (N53)

Bazel 9.3's common `toolchains` attribute and Make-variable expansion, as
genuine rule sets use them: aspect_rules_aws's `resolved_toolchain` (a rule
that forwards its resolved toolchain's `TemplateVariableInfo`), genrules such
as `~/code` `BUILD.bazel:449-462` (`cmd = "... $(AWS_CLI_BIN) ..."`,
`toolchains = ["@aspect_rules_aws//aws:resolved_toolchain"]`), `sh_binary`
with `toolchains`, and `src/beyondwork.ai/gogenrule.bzl`'s
`ctx.expand_make_variables("cmd", ctx.expand_location(ctx.attr.cmd), ctx.var)`.
Citations are to the pinned source `/tmp/opencode/bazel-9.3-reference`,
`src/main/java/com/google/devtools/build/lib/`.

Code: `src/location_expander.rs` (location functions), `src/ctx_surface.rs`
(`ctx.var`, TemplateExpander, `ctx.expand_location`), `src/analysis.rs`
(Starlark `toolchains` targets, `ctx.var` providers, the location map,
reported errors), `src/engine.rs` (edges, prerequisite checks, genrule
toolchain types, tool inputs and command expansion), `src/custom_rule.rs`
(`toolchains_attribute_spec`), `src/template_variable_info.rs` (the provider).

## The attribute

| Rule | Definition | Accepted prerequisites |
| --- | --- | --- |
| Starlark rules | `StarlarkRuleClassFunctions.baseRule` (`analysis/starlark/StarlarkRuleClassFunctions.java:159-194`, the `toolchains` entry `:165-168`), as `BaseRuleClasses.MakeVariableExpandingRule` (`analysis/BaseRuleClasses.java:352-364`) | rules with `TemplateVariableInfo`; no files (`FileTypeSet.NO_FILE`); `dontCheckConstraints` |
| `genrule` | `GenRuleBaseRule.java:322-333` (nonconfigurable) | rules with `TemplateVariableInfo` **or** `ToolchainTypeInfo`; no files |

* Dependencies are in the target configuration (no transition), checked for
  visibility and testonly like every prerequisite, after the rule's
  `package_metadata` and before its own attributes (Starlark rules).
* A rule without the provider: `in toolchains attribute of R rule L: 'X' does
  not have mandatory providers: 'TemplateVariableInfo'` (genrule: `...:
  'TemplateVariableInfo' or 'ToolchainTypeInfo'`;
  `RuleContext.checkRuleDependencyMandatoryProviders`,
  `analysis/RuleContext.java:2070-2095`, `packages/RequiredProviders.java:208-213, 308-321`),
  `X` the end of an alias chain; a file: `source file 'X' is misplaced here
  (expected no files)` / `generated file ...` — every error at the rule's
  location, all of them reported. A `toolchain_type` in a Starlark rule's
  `toolchains` is a missing-provider error (it is not a toolchain request).
* `ctx.attr.toolchains` are the Targets (aliases print as `<alias target A of
  L>`), `ctx.files.toolchains` their files to build.
* Genrule `toolchains` are also optional toolchain types
  (`ToolchainContextUtil.updateToolchainTypesFromAttribute`,
  `skyframe/toolchains/ToolchainContextUtil.java:61-64, 94-117`: `mandatory(false)`,
  `ignoreIfInvalid(true)`): a `toolchain_type` entry is resolved like a rule
  toolchain type (docs/TOOLCHAINS.md); other entries are silently ignored as
  types. Starlark rules never derive toolchain types from the attribute.
* `query` lists the edges (`deps(//gt:aws_script, 1)` includes
  `@aspect_rules_aws//aws:resolved_toolchain`; `deps()` of a genrule with a
  `toolchain_type` entry includes the type). Configured `cquery` is now supported
  separately; see [CQUERY.md](CQUERY.md).

## Make variables

`ConfigurationMakeVariableContext` (`analysis/ConfigurationMakeVariableContext.java:43-89`)
looks variables up, first match wins, in: `--define` values; the package
`vardef` environment (always empty in Bazel 9); the rule's template variables
(`RuleContext.getDefaultTemplateVariableProviders`, `analysis/RuleContext.java:900-940`:
the `TemplateVariableInfo` of the prerequisites of `toolchains`, `:cc_toolchain`,
`$toolchains`, `$cc_toolchain`, then of every resolved toolchain; the first
provider defining a name wins); the global environment `TARGET_CPU` (the
target platform's `@platforms//cpu` value), `COMPILATION_MODE`, `BINDIR`,
`GENDIR`. An unknown name is `$(NAME) not defined`; a function `$(name arg)`
is `$(name) not defined`.

* `ctx.var` is `collectMakeVariables` (`:75-83`): the suppliers in reverse
  order into a `Dict.Builder` (a name keeps its first position, the last
  value): `{"TARGET_CPU": ..., "COMPILATION_MODE": ..., "BINDIR": ...,
  "GENDIR": ..., <template variables, later providers win, sorted>, <defines>}`
  — for example `"COMPILATION_MODE": "tv_mode"` at the second position when a
  toolchain defines it.
* `ctx.expand_make_variables(attr, cmd, subst)` (`analysis/starlark/StarlarkRuleContext.java:950-978`)
  looks up `subst` first, then the context above; no location functions.
  Values are expanded recursively (Make's `:=`, depth 10: `potentially
  unbounded recursion during expansion of '$(B)'`); `$$` is `$`;
  `'$X'`/`'${X}' syntax is not supported; use '$(X)' instead ...`;
  `unterminated variable reference`, `unterminated $`
  (`analysis/stringtemplate/TemplateExpander.java:55-177`). Errors are
  attribute errors `in A attribute of R rule L: ...` at the rule's location,
  reported once per distinct text; the implementation receives the command
  unchanged and the target fails after it returns (`analysis/Expander.java:159-172`).

## Genrule commands

`GenRuleBase.create` (`rules/genrule/GenRuleBase.java:70-240`):

* **Tool dependencies** (`:96-110`, `CommandHelper`, `analysis/CommandHelper.java:128-181`):
  `tools` (exec configuration), `toolchains` and the genrule's resolved
  toolchain implementations (target configuration). Their files to build are
  action inputs (`:169-175`; never in `$(SRCS)`) — so a toolchain target's
  `DefaultInfo` files (the aws CLI binary of a `target_tool` toolchain) are
  inputs of the genrule; the paired capture checks this with a generated tool
  file the genrule reads. Each maps its label as written to its executable,
  else its files to build, in the location map.
* **One TemplateExpander pass** over `cmd` (`:122-134`) with
  `CommandResolverContext.lookupVariable` (`:327-400`): `SRCS`/`OUTS` (exec
  paths joined by a space, unescaped), `<`/`@` (`variable '$<' : no input
  file`, `... more than one input file`, `output file`), `RULEDIR`, `@D`; then
  the Make variables above (so `--define=SRCS=x` never replaces `$(SRCS)`).
  Functions are the location functions (`LocationTemplateContext.lookupFunction`,
  `analysis/LocationTemplateContext.java:100-120`, `Expander.withExecLocationsNoSrcs`,
  `analysis/Expander.java:100-106`); any other function is `$(name) not defined`.
  Errors are `in cmd attribute of genrule rule L: ...` at the rule's location.
* The location map (`LocationExpander.buildLocationMap`,
  `analysis/LocationExpander.java:401-485`, no data, no srcs collection): the
  `srcs` files under the label as written (`BazelGenRule.collectSources`,
  `bazel/rules/genrule/BazelGenRule.java:30-42`), the tool dependencies
  (`tools` also under the actual label) and the outputs.

## Location functions

`LocationExpander.allLocationFunctions` (`:369-391`) and `LocationFunction`
(`:240-362`): `location`/`locations` (exec paths for genrules and
`ctx.expand_location`; runfiles paths with `short_paths = True`),
`execpath(s)`, `rootpath(s)` (the short path, `../repo/...` outside the main
repository), `rlocationpath(s)` (`_main/...`, or `repo/...`). Paths are sorted
and de-duplicated, `PathFragment.getCallablePathString` (`./x` for a
single-segment path such as a root-package source, `vfs/PathFragment.java:735-745`)
and shell-escaped. Errors: `invalid label in $(location) expression: ...`,
`label 'L' in $(location) expression is not a declared prerequisite of this
rule`, `... expands to no files`, `... expands to more than one file, please
use $(locations L) instead.  Files (at most 5 shown) are: [a, b]` (the plural
functions say `$(locations)`).

`ctx.expand_location(input, targets = [], short_paths = False)`
(`analysis/starlark/StarlarkRuleContext.java:1029-1053`): only `$(FUNCTION `
prefixes of the eight functions are replaced (`$$`, `$(FOO)` stay;
`LocationExpander.expand`, `:172-224`); the map is `targets`
(`makeLabelMap`, `:1284-1313`: the label as written; the executable when the
files to build are not exactly one file; a label twice is `Label "L" is
found more than once in 'targets' list.`) plus the rule's outputs, its `srcs`
files and the executable (else files) of `deps`, `implementation_deps` and
`tools` — each only when declared as a label list (`withExecPaths`,
`:149-161`). A failure is a rule error `in R rule L: ...` and the input is
returned. `targets` absent is `[]`; an explicit None is `in call to
expand_location(), parameter 'targets' got value of type 'NoneType', want
'sequence'` (N50b; interned Targets and label-keyed dict merging:
docs/EXPAND_LOCATION.md).

## Evidence

Paired capture `artifacts/genrule_toolchains-capture.py` against genuine
Bazel 9.3.0 (tracked receipt `tests/fixtures/genrule_toolchains/reference_receipt.json`):
46 rows over the fixture `tests/fixtures/genrule_toolchains/workspace` (an
offline `aspect_rules_aws` stand-in whose `aws/toolchain.bzl` and
`aws/private/resolved_toolchain.bzl` are verbatim copies of aspect_rules_aws
0.8.3). Run 1: 40 EXACT, 1 EXACT_MESSAGE, 1 EXPECTED_NATIVE_UNSUPPORTED,
4 MISMATCH (errors reported by an implementation lacked the rule location and
repeated identical events), fixed before the single rerun. Run 2 (the
receipt): 44 EXACT, 1 EXACT_MESSAGE (the duplicate-`targets` Starlark
traceback framing), 1 EXPECTED_NATIVE_UNSUPPORTED (`cquery`), 0 MISMATCH.
Genuine Bazel confirms that a `toolchains` target's files are genrule inputs
(row A3) and that `$(location)` of a root-package file is `./root.txt` (A11;
the basic fixture's `//:copy` key was re-pinned for it). CLI tests:
`tests/genrule_toolchains.rs`.

Follow-up (review, `artifacts/genrule_toolchains-alias-chain-capture.py`, run
once, receipt `post_capture_changes.alias_chain_followup`): 6 EXACT rows show
that location maps hold an alias chain's written and actual labels only —
`AliasProvider.getDependencyLabels` (`analysis/AliasProvider.java:84-97`)
"returns at most two labels" — so `$(location)` of a middle alias is `not a
declared prerequisite of this rule`, and genrule `srcs` map only the written
label. Implementation-time and genrule events share one formatter,
`src/rule_errors.rs` (EventHandlingErrorReporter.prefixRuleMessage /
completeAttributeMessage, located and de-duplicated as
StarlarkRuleConfiguredTargetUtil.buildRule reports them, `:97-115`).

## Not implemented / divergences

* The historical capture's `cquery` gap is now covered by configured analysis
  (docs/CQUERY.md); `query_and_cquery_list_toolchain_edges` checks both commands.
* A Starlark traceback keeps the native format (the duplicate-`targets`
  message is Bazel's text).
* `heuristic_label_expansion = True`, `cmd_bash`, Windows commands and
  commands longer than 64000 characters (Bazel's script file) still fail
  explicitly (docs/NATIVE_BUILD_RULES.md). Tool runfiles trees are not
  staged (docs/RUNFILES.md).
* `RuleClass.checkForDuplicateLabels` for `toolchains` remains the shared
  label-list gap (docs/COMMON_RULE_ATTRIBUTES.md).
