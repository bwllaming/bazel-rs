load(":defs.bzl", "probe", "show_test")

[config_setting(
    name = name,
    values = {option: value},
) for name, option, value in [
    ("classpath_off", "experimental_java_classpath", "off"),
    ("classpath_javabuilder", "experimental_java_classpath", "javabuilder"),
    ("classpath_bazel", "experimental_java_classpath", "bazel"),
    ("lint", "experimental_run_android_lint_on_java_rules", "true"),
]]

FLAGS = select({
    ":classpath_off": ["classpath_off"],
    ":classpath_javabuilder": ["classpath_javabuilder"],
    ":classpath_bazel": ["classpath_bazel"],
    "//conditions:default": [],
}) + select({
    ":lint": ["lint"],
    "//conditions:default": [],
})

probe(
    name = "exec_probe",
    flags = FLAGS,
)

probe(
    name = "plain",
    flags = FLAGS,
    tool = ":exec_probe",
)

show_test(name = "shown", target = ":plain")

platform(name = "p")
