"""An os- and arch-dependent extension."""

load("//:repo.bzl", "gen_repo")

def _plat_impl(ctx):
    print("LOCKEXT_EVAL plat")
    gen_repo(name = "gen_plat", s = ctx.os.name + "/" + ctx.os.arch)

plat = module_extension(
    implementation = _plat_impl,
    os_dependent = True,
    arch_dependent = True,
)
