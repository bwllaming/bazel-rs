DEFINITION_LABEL = Label(":source.txt")

def source_attribute():
    # The attr descriptor is created inside this helper module, rather than
    # the module that defines rule() or the package that invokes the rule.
    return attr.label(default = ":source.txt", allow_single_file = True)
