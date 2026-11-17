def _impl(name, visibility, **kwargs):
    print("PRL", native.package_relative_label(":x"), native.package_relative_label("//q:y"), native.package_name())
    native.genrule(name = "lib" + name, outs = ["lib" + name + ".out"], cmd = "touch $@")
    native.genrule(name = name, outs = [name + ".out"], cmd = "touch $@", visibility = visibility)
m = macro(implementation = _impl, inherit_attrs = "common")
