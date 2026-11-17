def _out(name, visibility):
    outer(name = name + '_a')
outer = macro(implementation = _out)
