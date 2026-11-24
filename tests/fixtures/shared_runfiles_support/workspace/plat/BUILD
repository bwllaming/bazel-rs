constraint_setting(name = "flavor")

constraint_value(name = "a", constraint_setting = ":flavor", visibility = ["//visibility:public"])

platform(
    name = "host",
    constraint_values = [":a"],
)
