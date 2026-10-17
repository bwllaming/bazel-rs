load("@rules_shell//shell:sh_binary.bzl", "sh_binary")
load("@rules_shell//shell:sh_library.bzl", "sh_library")
load(":defs.bzl", "show_env")

sh_library(name = "lib", srcs = ["lib.sh"], data = ["data.txt"])

sh_binary(
    name = "envloc",
    srcs = ["bin.sh"],
    data = ["greeting.txt", "@bazel_tools//tools/test:collect_coverage"],
    deps = [":lib"],
    env = {
        "GREETING_FILE": "$(rlocationpath greeting.txt)",
        "GREETING_ROOT": "$(rootpath :greeting.txt)",
        "COVERAGE": "$(rlocationpath @bazel_tools//tools/test:collect_coverage)",
        "SRC": "$(execpath bin.sh)",
        "LIBS": "$(rootpaths :lib)",
        "MIXED": "$$HOME $(location greeting.txt)",
    },
)

sh_binary(name = "envbad", srcs = ["bin.sh"], env = {"X": "$(rlocationpath :nope)"})

show_env(name = "show", dep = ":envloc")
