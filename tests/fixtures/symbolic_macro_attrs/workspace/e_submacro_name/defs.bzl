def _in(name, visibility):
    pass
inner = macro(implementation = _in)
def _out(name, visibility):
    inner(name = 'other')
outer = macro(implementation = _out)
