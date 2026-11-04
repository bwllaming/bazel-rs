"""Re-exports of bazel_skylib's lib modules."""

load("@bazel_skylib//lib:collections.bzl", _collections = "collections")
load("@bazel_skylib//lib:dicts.bzl", _dicts = "dicts")
load("@bazel_skylib//lib:partial.bzl", _partial = "partial")
load("@bazel_skylib//lib:paths.bzl", _paths = "paths")
load("@bazel_skylib//lib:selects.bzl", _selects = "selects")
load("@bazel_skylib//lib:sets.bzl", _sets = "sets")
load("@bazel_skylib//lib:shell.bzl", _shell = "shell")
load("@bazel_skylib//lib:structs.bzl", _structs = "structs")
load("@bazel_skylib//lib:types.bzl", _types = "types")
load("@bazel_skylib//lib:versions.bzl", _versions = "versions")
load("@bazel_skylib//lib:unittest.bzl", _analysistest = "analysistest", _asserts = "asserts", _unittest = "unittest", _loadingtest = "loadingtest")

collections = _collections
dicts = _dicts
partial = _partial
paths = _paths
selects = _selects
sets = _sets
shell = _shell
structs = _structs
types = _types
versions = _versions
analysistest = _analysistest
asserts = _asserts
unittest = _unittest
loadingtest = _loadingtest
