"""F3: DottedVersion matrix and a host_xcodes-like XcodeVersionInfo dump."""

dv = apple_common.dotted_version

PARSE = [
    "1", "1.2", "1.2.0", "1.0.0.0", "0", "0.0", "01.002", "5beta2", "5BETA2",
    "1.2beta", "1.2.beta", "1.2.beta.!!", "1.2.Experimental_3", "1.x.y", "1a2b",
    "1a2b34", "3.1.0alpha5.6", "7.3alpha.2", "15.0.1", "2147483647",
    "0000000000002147483647", "8.4", "10.11",
]

PAIRS = [
    ("3.1.25", "3.1.1"), ("3.1.20", "3.1.2"), ("3.1.1", "3.1"), ("3.1", "3.1.0.0"),
    ("3.2", "3.1.8"), ("3.1.1", "3.1.1beta3"), ("3.1.1beta1", "3.1.0"),
    ("3.1", "3.1.0alpha1"), ("3.1.0beta0", "3.1.0alpha5.6"),
    ("3.4.2alpha2", "3.4.2alpha1"), ("3.4.2alpha2", "3.4.2alpha1.5"),
    ("3.1alpha1", "3.1alpha"), ("1beta", "1Beta"), ("01.2", "1.2.0"), ("1.2", "1.2"),
    ("1.0beta", "1"), ("1.2.beta", "1.2"), ("0", "0.0.0"), ("1.2.3", "1.2.4"),
    ("10", "9.99"), ("1a", "1"), ("1a0", "1a"), ("2147483647", "2147483646"),
]

def _matrix():
    lines = []
    for s in PARSE:
        v = dv(s)
        lines.append("parse %r str=%s repr=%s to_string=%r type=%s dir=%s" % (
            s, str(v), repr(v), v.to_string, type(v), dir(v)))
    for a, b in PAIRS:
        x, y = dv(a), dv(b)
        lines.append("pair %r %r compare_to=%d reverse=%d eq=%s ne=%s lt=%s le=%s gt=%s ge=%s hash=%s" % (
            a, b, x.compare_to(y), y.compare_to(x), x == y, x != y, x < y, x <= y, x > y, x >= y,
            {x: "hit"}.get(y, "miss")))
    v = dv("1.2")
    lines.append("method repr=%s type=%s" % (repr(v.compare_to), type(v.compare_to)))
    lines.append("other eq_str=%s in_list=%s bool=%s named=%s" % (
        v == "1.2", v in [dv("1.2.0")], bool(v), dv(version = "1.2") == v))
    order = sorted([dv(s) for s in ["10", "9.1", "9.1beta", "9.1.0", "1.0alpha", "1"]])
    lines.append("sorted %s" % [str(x) for x in order])
    lines.append("max %s min %s" % (max(dv("2.0"), dv("10")), min(dv("2.0"), dv("10"))))
    lines.append("hasattr %s %s %s" % (hasattr(v, "to_string"), hasattr(v, "compare_to"), hasattr(v, "nope")))
    return lines

def _dotted_version_or_default(field, default):
    # apple_support xcode/xcode_config.bzl:399-400
    return apple_common.dotted_version(field) or default

def _host_xcodes():
    # apple_support xcode/xcode_config.bzl:76-113 with no versions:
    # XcodeVersionPropertiesInfo(xcode_version = None) defaults, every
    # apple fragment flag unset and cpp minimum_os_version() None.
    ios_sdk_version = _dotted_version_or_default("8.4", "8.4")
    macos_sdk_version = _dotted_version_or_default("10.11", "10.11")
    tvos_sdk_version = _dotted_version_or_default("9.0", "9.0")
    watchos_sdk_version = _dotted_version_or_default("2.0", "2.0")
    visionos_sdk_version = _dotted_version_or_default("1.0", "1.0")
    return apple_common.XcodeVersionConfig(
        ios_sdk_version = str(ios_sdk_version),
        ios_minimum_os_version = str(ios_sdk_version),
        visionos_sdk_version = str(visionos_sdk_version),
        visionos_minimum_os_version = str(visionos_sdk_version),
        watchos_sdk_version = str(watchos_sdk_version),
        watchos_minimum_os_version = str(watchos_sdk_version),
        tvos_sdk_version = str(tvos_sdk_version),
        tvos_minimum_os_version = str(tvos_sdk_version),
        macos_sdk_version = str(macos_sdk_version),
        macos_minimum_os_version = str(macos_sdk_version),
        xcode_version = None,
        availability = "UNKNOWN",
        xcode_version_flag = None,
        include_xcode_execution_info = False,
    )

def _local():
    return apple_common.XcodeVersionConfig(
        ios_sdk_version = "17.0",
        ios_minimum_os_version = "15.0",
        visionos_sdk_version = "1.0",
        visionos_minimum_os_version = "1.0",
        watchos_sdk_version = "10.0",
        watchos_minimum_os_version = "8.0",
        tvos_sdk_version = "17.0",
        tvos_minimum_os_version = "15.0",
        macos_sdk_version = "14.0",
        macos_minimum_os_version = "12.0",
        xcode_version = "15.0.1",
        availability = "LOCAL",
        xcode_version_flag = "15.0.1-local",
        include_xcode_execution_info = True,
    )

def info_lines(name, info):
    pt = apple_common.platform_type
    p = apple_common.platform
    lines = ["%s type=%s dir=%s" % (name, type(info), dir(info))]
    for t in dir(pt):
        lines.append("%s minimum_os_for_platform_type(%s)=%s" % (
            name, t, info.minimum_os_for_platform_type(getattr(pt, t))))
    for n in dir(p):
        lines.append("%s sdk_version_for_platform(%s)=%s" % (
            name, n, info.sdk_version_for_platform(getattr(p, n))))
    lines.append("%s availability=%r xcode_version=%r" % (name, info.availability(), info.xcode_version()))
    lines.append("%s execution_info=%r" % (name, info.execution_info()))
    for field in dir(info):
        lines.append("%s %s repr=%s type=%s" % (name, field, repr(getattr(info, field)), type(getattr(info, field))))
    return lines

def _dump_impl(ctx):
    lines = _matrix()
    lines.append("provider repr=%s type=%s" % (repr(apple_common.XcodeVersionConfig), type(apple_common.XcodeVersionConfig)))
    host = _host_xcodes()
    lines.extend(info_lines("host_xcodes", host))
    lines.extend(info_lines("local", _local()))
    lines.append("min_os_type=%s" % type(host.minimum_os_for_platform_type("ios")))
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out])), host]

dump = rule(implementation = _dump_impl)

def _consume_impl(ctx):
    info = ctx.attr.dep[apple_common.XcodeVersionConfig]
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "consumed in=%s ios=%s macos_sdk=%s availability=%r xcode_version=%r\n" % (
        apple_common.XcodeVersionConfig in ctx.attr.dep,
        info.minimum_os_for_platform_type(apple_common.platform_type.ios),
        info.sdk_version_for_platform(apple_common.platform.macos),
        info.availability(),
        info.xcode_version(),
    ))
    return [DefaultInfo(files = depset([out]))]

consume = rule(
    implementation = _consume_impl,
    attrs = {"dep": attr.label(providers = [apple_common.XcodeVersionConfig])},
)

def _bad_kwargs(**overrides):
    kwargs = dict(
        ios_sdk_version = "8.4",
        ios_minimum_os_version = "8.4",
        visionos_sdk_version = "1.0",
        visionos_minimum_os_version = "1.0",
        watchos_sdk_version = "2.0",
        watchos_minimum_os_version = "2.0",
        tvos_sdk_version = "9.0",
        tvos_minimum_os_version = "9.0",
        macos_sdk_version = "10.11",
        macos_minimum_os_version = "10.11",
        xcode_version = None,
        availability = "UNKNOWN",
        xcode_version_flag = None,
        include_xcode_execution_info = False,
    )
    kwargs.update(overrides)
    return apple_common.XcodeVersionConfig(**kwargs)

ERRORS = {
    "none": lambda: dv(None),
    "int": lambda: dv(1),
    "empty": lambda: dv(""),
    "double_dot": lambda: dv("1..2"),
    "trailing_dot": lambda: dv("1."),
    "descriptive_only": lambda: dv("beta"),
    "hyphen": lambda: dv("1.2-3"),
    "overflow": lambda: dv("2147483648"),
    "overflow_second": lambda: dv("1a2147483648"),
    "space": lambda: dv(" 1"),
    "unicode": lambda: dv("1.é"),
    "missing": lambda: dv(),
    "two_positional": lambda: dv("1", "2"),
    "unexpected_keyword": lambda: dv(versio = "1"),
    "compare_to_string": lambda: dv("1").compare_to("1"),
    "compare_to_named": lambda: dv("1").compare_to(other = dv("1")),
    "compare_to_missing": lambda: dv("1").compare_to(),
    "less_than_int": lambda: dv("1") < 1,
    "int_less_than": lambda: 1 < dv("1"),
    "no_field": lambda: dv("1").nope,
    "info_no_args": lambda: apple_common.XcodeVersionConfig(),
    "info_positional": lambda: apple_common.XcodeVersionConfig("8.4"),
    "info_bad_sdk": lambda: _bad_kwargs(ios_sdk_version = "8..4"),
    "info_none_sdk": lambda: _bad_kwargs(macos_sdk_version = None),
    "info_bad_xcode_version": lambda: _bad_kwargs(xcode_version = "x").xcode_version(),
    "info_path_xcode_version": lambda: fail("path=" + _bad_kwargs(xcode_version = "/Applications/Xcode.app").xcode_version()),
    "info_platform_type": lambda: _bad_kwargs().minimum_os_for_platform_type("beos"),
    "info_platform": lambda: _bad_kwargs().sdk_version_for_platform(struct(name = "x")),
}

def _err_impl(ctx):
    ERRORS[ctx.label.name]()
    return []

err = rule(implementation = _err_impl)
