# Shape of rules_go go/toolchain/toolchains.bzl declare_constraints()
# (rules_go 0.63, Apache-2.0), reduced to a few platforms.
GOOS_CONSTRAINTS = {
    "darwin": "@platforms//os:osx",
    "ios": "@platforms//os:ios",
    "linux": "@platforms//os:linux",
    "windows": "@platforms//os:windows",
    "js": "//go/toolchain:js",
}

GOARCH_CONSTRAINTS = {
    "amd64": "@platforms//cpu:x86_64",
    "arm64": "@platforms//cpu:aarch64",
    "wasm": "//go/toolchain:wasm",
}

PLATFORMS = [
    struct(name = "linux_amd64", constraints = ["//go/toolchain:linux", "//go/toolchain:amd64"]),
    struct(name = "linux_amd64_cgo", constraints = ["//go/toolchain:linux", "//go/toolchain:amd64", "//go/toolchain:cgo_on"]),
    struct(name = "js_wasm", constraints = ["//go/toolchain:js", "//go/toolchain:wasm"]),
]

def declare_constraints():
    for goos, constraint in GOOS_CONSTRAINTS.items():
        if constraint.startswith("//go/toolchain:"):
            native.constraint_value(
                name = goos,
                constraint_setting = "@platforms//os:os",
            )
        else:
            native.alias(
                name = goos,
                actual = constraint,
            )

    for goarch, constraint in GOARCH_CONSTRAINTS.items():
        if constraint.startswith("//go/toolchain:"):
            native.constraint_value(
                name = goarch,
                constraint_setting = "@platforms//cpu:cpu",
            )
        else:
            native.alias(
                name = goarch,
                actual = constraint,
            )

    native.constraint_setting(
        name = "cgo_constraint",
    )

    native.constraint_value(
        name = "cgo_on",
        constraint_setting = ":cgo_constraint",
    )

    native.constraint_value(
        name = "cgo_off",
        constraint_setting = ":cgo_constraint",
    )

    for p in PLATFORMS:
        native.platform(
            name = p.name,
            constraint_values = p.constraints,
        )

def declare_toolchains(prefix, goos, goarch):
    native.toolchain(
        name = prefix + "_toolchain",
        toolchain_type = "//go:toolchain",
        exec_compatible_with = ["//go/toolchain:" + goos, "//go/toolchain:" + goarch],
        target_compatible_with = ["//go/toolchain:" + goos, "//go/toolchain:" + goarch],
        toolchain = ":" + prefix + "_sdk",
    )
