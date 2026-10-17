"""A non-reproducible extension returning root module direct deps metadata."""

load("//:repo.bzl", "gen_repo")

def _meta_impl(ctx):
    print("LOCKEXT_EVAL meta")
    gen_repo(name = "gen_meta", b = True)
    return ctx.extension_metadata(
        root_module_direct_deps = ["gen_meta"],
        root_module_direct_dev_deps = [],
    )

meta = module_extension(implementation = _meta_impl)
