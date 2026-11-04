genrule(name = "gen", outs = ["gen.sh"], cmd = "echo tool-talks >&2; printf '#!/bin/sh\\necho generated\\n' > $@; chmod +x $@", executable = True, visibility = ["//visibility:public"])
load("//:defs.bzl", "toolrule")
toolrule(name = "tr", visibility = ["//visibility:public"])
