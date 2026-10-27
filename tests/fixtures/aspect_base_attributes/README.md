# aspect_base_attributes

`ctx.rule.attr` of an aspect also holds the attributes of the base aspects of
its path (StarlarkRuleContext.java:302-316 over AspectContext's rule-and-base-
aspects prerequisites, DependencyResolutionHelpers.getAttributes/
addAspectAttributes): `main` requires `base` (implicit label attributes: an
exec tool, a file list with a generated file, a single file; a `flavor`
parameter; private strings) and sees `seen` through
`required_aspect_providers`. The base rule's attributes win a name clash
(`_rimpl` is the rule's label), then the base aspects in path order
(`_shared` is `base`'s); the main aspect's own attributes are only in
`ctx.attr`; label-typed base-aspect attributes are dependencies of the main
aspect's application; a base aspect propagates along the main aspect's own
attributes (`_mdep` gets `base`). The second command gives `base` a
top-level parameter. `genuine.txt` holds the genuine 9.3.0 capture;
tests/aspects.rs `ctx_rule_attr_holds_base_aspect_attributes` compares the
native prints with it.
