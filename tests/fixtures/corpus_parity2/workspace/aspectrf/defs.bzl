# ctx.runfiles in an aspect implementation (protobuf's rust proto aspects
# through rules_rust's rustc_compile_action).
def _a_impl(target, ctx):
    r = ctx.runfiles(files = ctx.rule.files.srcs, symlinks = {"link": ctx.rule.files.srcs[0]})
    print("aspect runfiles: %s %s" % (
        [f.short_path for f in r.files.to_list()],
        [(s.path, s.target_file.short_path) for s in r.symlinks.to_list()],
    ))
    return []

a = aspect(implementation = _a_impl)

def _r_impl(ctx):
    return []

r = rule(implementation = _r_impl, attrs = {"deps": attr.label_list(aspects = [a])})
