ROOT_TOOLS = Label("@bazel_tools//tools:host_platform")
ROOT = [str(ROOT_TOOLS), ROOT_TOOLS.repo_name]
