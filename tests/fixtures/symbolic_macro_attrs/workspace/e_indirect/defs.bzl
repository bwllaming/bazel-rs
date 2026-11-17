def _in(name, visibility):
    outer(name = name + '_b')
inner = macro(implementation = _in)
def _out(name, visibility):
    inner(name = name + '_a')
outer = macro(implementation = _out)
