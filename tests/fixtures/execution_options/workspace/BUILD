load(":defs.bzl", "my_toolchain", "use")
toolchain_type(name = "t")
constraint_setting(name = "os")
constraint_value(name = "linux", constraint_setting = ":os")
constraint_value(name = "mac", constraint_setting = ":os")
constraint_setting(name = "flavor", default_constraint_value = ":plain")
constraint_value(name = "plain", constraint_setting = ":flavor")
constraint_value(name = "fancy", constraint_setting = ":flavor")
platform(name = "p", constraint_values = [":linux"])
platform(name = "e2", constraint_values = [":mac"])
my_toolchain(name = "mac_impl")
my_toolchain(name = "linux_impl")
my_toolchain(name = "fancy_impl")
toolchain(name = "mac_tc", toolchain = ":mac_impl", toolchain_type = ":t", target_compatible_with = [":mac"])
toolchain(name = "fancy_tc", toolchain = ":fancy_impl", toolchain_type = ":t", target_compatible_with = [":linux", ":fancy"])
toolchain(name = "linux_tc", toolchain = ":linux_impl", toolchain_type = ":t", target_compatible_with = [":linux"], exec_compatible_with = [":linux"])
use(name = "u")
use(name = "v", exec_compatible_with = [":mac"])

genrule(
    name = "g",
    outs = ["g.txt"],
    cmd = "echo hi > $@; echo warn-out; echo warn-err >&2",
)

genrule(
    name = "h",
    srcs = [":g"],
    outs = ["h.txt"],
    cmd = "cat $< > $@",
)
