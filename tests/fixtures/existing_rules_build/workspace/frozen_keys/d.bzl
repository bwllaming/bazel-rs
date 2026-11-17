def m():
    native.genrule(name = "g", outs = ["g.out"], cmd = "x")
    native.existing_rule("g").keys().append(1)
