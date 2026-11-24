def _impl(ctx):
    return [platform_common.ToolchainInfo()]

tc_impl = rule(_impl)
