load("@rules_shell//shell:sh_binary.bzl", "sh_binary")
load("@rules_shell//shell:sh_library.bzl", "sh_library")
load("@rules_shell//shell:sh_test.bzl", "sh_test")
package(default_visibility = ["//visibility:public"])

sh_library(name = "lib", srcs = ["lib.sh"], data = ["data.txt"])

sh_binary(name = "bin", srcs = ["bin.sh"], deps = [":lib"], data = ["greeting.txt"], env = {"GREETING": "hi"}, env_inherit = ["HOME"])

sh_binary(name = "launcher", srcs = ["bin.sh"], deps = [":lib"], data = ["greeting.txt"], use_bash_launcher = True)

sh_test(name = "t", srcs = ["t.sh"], deps = [":lib"], data = ["greeting.txt"])
