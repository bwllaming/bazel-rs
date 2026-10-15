"""Shared providers (pattern: rules_go go/private/providers.bzl, protobuf bazel/common/proto_info.bzl)."""

GoInfo = provider(doc = "Go package metadata", fields = ["importpath"])
ProtoInfo = provider(doc = "Proto sources", fields = ["srcs"])
CcLikeInfo = provider(fields = ["headers"])
