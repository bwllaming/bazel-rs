"""A reproducible extension: never recorded in MODULE.bazel.lock."""

load("//:repo.bzl", "gen_repo")

def _repro_impl(ctx):
    print("LOCKEXT_EVAL repro")
    gen_repo(name = "gen_repro", i = 42)
    return ctx.extension_metadata(reproducible = True)

repro = module_extension(implementation = _repro_impl)
