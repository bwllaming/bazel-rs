def _impl(ctx):
    fail("repo boom")
r = repository_rule(implementation = _impl)
