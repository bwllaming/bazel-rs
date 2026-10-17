# A non-root .bzl loading the built-in repository's contents.
load("@bazel_tools//tools/build_defs/repo:utils.bzl", "maybe")

LOADED = [str(type(maybe))]
