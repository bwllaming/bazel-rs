# N58 Starlark aspects fixture

`workspace/` holds the fixture the paired capture
(artifacts/aspects-capture.py) and tests/aspects.rs run: aspect
definition parameters, attr(aspects=...) propagation, aspect providers
merged over the base configured target's (MergedConfiguredTarget),
aspect-on-aspect through required_aspect_providers, `--aspects` (with
--aspects_parameters and the ValidateTarget aspect of
--experimental_use_validation_aspect) and the aspect context
(ctx.attr of the aspect's own attributes, ctx.rule over the base target's).
