def _impl(ctx):
    fail("ext boom")
e = module_extension(implementation = _impl)
