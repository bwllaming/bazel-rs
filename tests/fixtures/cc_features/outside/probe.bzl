"""A feature-configuration caller outside the private-API allowlists."""

load("@rules_cc//cc:cc_toolchain_config_lib.bzl", "feature")
load("//tools/build_defs/cc:probe.bzl", "NO_LEGACY", "configure", "toolchain")

def _outside_impl(ctx):
    fc = configure(toolchain(ctx, [NO_LEGACY, feature(name = "x")]), ["x"])
    fc.is_enabled("x")
    return []

outside_probe = rule(implementation = _outside_impl)
