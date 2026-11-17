def _flag_impl(ctx):
    return []

string_flag = rule(_flag_impl, build_setting = config.string(flag = True))
