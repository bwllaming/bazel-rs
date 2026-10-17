# Label resolution in a non-root module: @bazel_tools is in its mapping.
TOOLS = Label("@bazel_tools//tools:host_platform")
SELF = Label("@localdep//:labels.bzl")
NON_ROOT = [str(TOOLS), TOOLS.repo_name, str(SELF)]
