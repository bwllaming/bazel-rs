FnInfo = provider(fields = ["fn", "closure"])
def _dirname(file):
    return file.dirname
def _tc_impl(ctx):
    def inner(file):
        return file.basename
    return [FnInfo(fn = _dirname, closure = inner)]
tc = rule(_tc_impl)
