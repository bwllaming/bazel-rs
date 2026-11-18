# Rule extension: `rule(parent = ...)`

B17 in docs/OSS_CORPUS.md (with_cfg.bzl, rules_cc). Genuine 9.3.1 capture:
`tests/fixtures/rule_extension` (tests/rule_extension.rs).

## Definition (StarlarkRuleClassFunctions.rule/createRule, RuleClass.Builder(parent))

- The parent must be an exported Starlark rule that is extendable
  (RuleClass.isExtendable: not `extendable = False`, not
  `output_to_genfiles`, `_skylark_testable`, an analysis test or a build
  setting). `executable`, `test`, `outputs`, `host_fragments`,
  `output_to_genfiles`, `_skylark_testable`, `analysis_test` and
  `build_setting` may not be given; genuine's texts.
- The child is executable / a test as its parent and inherits, in this
  order, the parent's attributes (schema, attribute order, computed
  defaults, attribute transitions, lifted subrule attributes, allowlists),
  subrules, fragments, toolchain types, exec groups (the child's override
  same-named ones), `exec_compatible_with`, advertised providers, implicit
  outputs and rule `cfg`.
- A child attribute with an inherited name is an override
  (RuleClass.Builder.override, Attribute.failIfNotAValidOverride): only a
  public label / label_list of the same type, setting nothing but a
  default, doc or aspects. The result is the parent's attribute with the
  child's default (unless the child's is null) and the aspects added.
- A parent with an extend-rule allowlist (its `extendable` label, or the
  default public `@bazel_tools//tools/allowlists/extend_rule_allowlist`
  when `extendable` was not given) adds `_allowlist_extend_rule`
  (`cfg = "exec"`) to the child; a target whose rule's definition package is
  not in the group fails `Non-allowlisted attempt to extend a rule.`
  (LocationCheck.DEFINITION).
- A Starlark `cfg` on both the child and an ancestor composes, child first
  (ComposingTransitionFactory.of(child, parent)): the child's transition is
  the rule's `""` transition, the ancestors' follow as `^1`, `^2`, ...
  (custom_rule::composed_rule_transition_key), applied in turn by the rule
  transition (and again for the idempotency check).
- Not modelled: composing `config.none()` with another rule transition;
  overriding an attribute with a computed default. Both fail explicitly.

## Instantiation

The initializers run child to ancestor (StarlarkRuleFunction.call), each
over the attributes its rule class knows; an ancestor's label results are
reified relative to its own `.bzl`.

## Analysis (StarlarkRuleContext.callParent)

The child's implementation runs; `ctx.super()` evaluates the next
ancestor's implementation on the same context and returns its providers as
a list (a single provider is wrapped, `None` is `[]`, a struct fails).
Errors: `Can't use 'super' call, the rule has no parent.`, `'super' called
the second time.`, the rule errors `'super' was not called.` (top level)
and `in R rule: 'super' was not called.` (an ancestor R), and `Error
evaluating parent rule.` after an ancestor's evaluation error.
