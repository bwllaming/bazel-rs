# aspect_parameters

An aspect with two parameter attributes (`p`, `n`, both with `values`)
attached to `deps` by rules that have same-named attributes (`r`: `p`/`n`
with defaults `y`/`1`, `only_p`: `d`/`0`), propagating through plain rules.
Pins StarlarkDefinedAspect.getDefaultParametersExtractor (an attribute's
aspect takes its rule's values; its descriptor shows them) and descriptor
identity: two rules with different values are two applications on the
shared dependencies, the same aspect from `--aspects` (every parameter, given
or default) and from an attribute is two applications when the parameters
differ and one when they are equal. `genuine.txt` holds the genuine 9.3.0
capture; tests/aspects.rs `attribute_aspects_take_their_rules_parameters`
compares the native prints with it.
