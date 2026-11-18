# B41: rules_js's package links: an unresolved symlink artifact whose
# relative text names another action's output (a tree) resolves in
# bazel-bin.
def _impl(ctx):
    store = ctx.actions.declare_directory("store/node_modules/pkg")
    ctx.actions.run_shell(
        outputs = [store],
        command = "mkdir -p $1 && echo pkg > $1/index.js",
        arguments = [store.path],
    )
    link = ctx.actions.declare_symlink("app/node_modules/pkg")
    ctx.actions.symlink(output = link, target_path = "../../store/node_modules/pkg")
    dangling = ctx.actions.declare_symlink("app/dangling")
    ctx.actions.symlink(output = dangling, target_path = "../nowhere/at/all")
    return [DefaultInfo(files = depset([store, link, dangling]))]

links = rule(implementation = _impl)
