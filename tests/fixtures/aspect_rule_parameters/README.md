# aspect_rule_parameters

The rule-side check of attribute-attached aspects' parameters, made when the
rule is instantiated (StarlarkRuleClassFunctions.validateRulePropagatedAspects,
AspectsList.validateRulePropagatedAspectsParameters): `req`'s required
parameters `p` (no default) and `s` (empty default) must be same-typed
attributes of the propagating rule (`good` has both; `wrong_type`'s `p` is a
string, `missing` lacks `s`); `novals`'s int parameter has no `values`;
`flag`'s bool parameter (default False) is required too. `genuine.txt`
holds the genuine 9.3.0 capture; tests/aspects.rs
`rules_must_declare_their_aspects_required_parameters` compares the native
run.
