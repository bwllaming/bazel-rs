"""Helpers loaded by ext.bzl; top-level Label() records a repo mapping."""

TOOLS = Label("@bazel_tools//tools/build_defs/repo:http.bzl")

def describe(tag):
    return "%s:%d" % (tag.id, tag.count)
