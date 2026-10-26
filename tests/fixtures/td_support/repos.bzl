def _repo(ctx):
    fail("mapping must not materialize the repository")

_generated = repository_rule(implementation = _repo)

def _ext(ctx):
    _generated(name = "generated")

ext = module_extension(implementation = _ext)
