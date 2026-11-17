def inner():
    native.existing_rule("x")

def _impl(name, visibility):
    inner()

outer = macro(implementation = _impl)
