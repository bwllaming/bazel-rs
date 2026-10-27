# aspect_propagation_functions

Function-valued `attr_aspects` and `propagation_predicate`
(AspectPropagationEdgesSupplier, AspectPropagationPredicate,
StarlarkAspectPropagationContext): `fa` computes the attribute to
propagate along from its `mode` parameter (`deps` at the top level, the
rule's `extra` through `top`'s attribute) unless the target's `s` is
`stop`, and its predicate drops targets tagged `skip` (also a top-level
one). The functions print their `propagation_ctx` (aspect parameters,
`rule.label`, `rule.qualified_kind`, `rule.attr.<name>.value/is_tool`,
types). `bad_list`, `bad_star` and `bad_pred` return invalid values.
`genuine.txt` holds the genuine 9.3.0 capture (sorted, deduplicated
prints, or the error); tests/aspects.rs `aspect_propagation_functions`
compares the native run with it.
