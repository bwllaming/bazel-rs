load("//lk:defs.bzl", "lk_rules")
genrule(name = "g", outs = ["g.txt"], cmd = "echo > $@")
alias(name = "ga", actual = ":g")
lk_rules(name = "bad", m = {":g": "x", ":ga": "y"})
