def m():
    native.genrule(name = "g", outs = ["g.out"], cmd = "x")
    x = {native.existing_rule("g"): 1}
