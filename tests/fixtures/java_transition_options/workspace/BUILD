load(":defs.bzl", "probe", "set_java_options", "show_test")

filegroup(name = "launcher")

filegroup(name = "host_launcher")

[config_setting(
    name = name,
    values = {option: value},
) for name, option, value in [
    ("no_header", "java_header_compilation", "false"),
    ("no_ijars", "use_ijars", "false"),
    ("no_deps", "java_deps", "false"),
    ("one_version_warning", "experimental_one_version_enforcement", "warning"),
    ("no_tests", "one_version_enforcement_on_java_tests", "false"),
    ("no_exports", "incompatible_disallow_java_import_exports", "true"),
    ("warn", "experimental_strict_java_deps", "warn"),
    ("launcher_set", "java_launcher", "//:launcher"),
]]

FLAGS = [select({
    ":" + name: [name],
    "//conditions:default": [],
}) for name in [
    "no_header",
    "no_ijars",
    "no_deps",
    "one_version_warning",
    "no_tests",
    "no_exports",
    "warn",
    "launcher_set",
]]

probe(
    name = "exec_probe",
    flags = FLAGS[0] + FLAGS[1] + FLAGS[2] + FLAGS[3] + FLAGS[4] + FLAGS[5] + FLAGS[6] + FLAGS[7],
)

probe(
    name = "plain",
    flags = FLAGS[0] + FLAGS[1] + FLAGS[2] + FLAGS[3] + FLAGS[4] + FLAGS[5] + FLAGS[6] + FLAGS[7],
)

probe(
    name = "transitioned",
    flags = FLAGS[0] + FLAGS[1] + FLAGS[2] + FLAGS[3] + FLAGS[4] + FLAGS[5] + FLAGS[6] + FLAGS[7],
    tool = ":exec_probe",
)

set_java_options(name = "java", probes = [":transitioned"])

show_test(name = "shown", target = ":plain")

platform(name = "p")
