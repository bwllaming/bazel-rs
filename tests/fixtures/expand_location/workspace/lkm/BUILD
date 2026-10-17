load(":defs.bzl", "lk")

config_setting(name = "opt", values = {"compilation_mode": "opt"})

[filegroup(name = n, srcs = []) for n in ["p1", "p2", "p3", "p4"]]

lk(name = "sel_then_dict", m = select({":opt": {":p1": "opt1", ":p4": "opt4"}, "//conditions:default": {":p1": "sel1", ":p2": "sel2"}}) | {":p3": "dict3", "//lkm:p2": "dict2", ":p1": "dict1"})

lk(name = "dict_then_sel", m = {":p3": "dict3", ":p2": "dict2"} | select({":opt": {":p4": "opt4"}, "//conditions:default": {":p2": "sel2", ":p1": "sel1"}}))

lk(name = "default_only", m = select({"//conditions:default": {":p1": "sel1", ":p2": "sel2"}}) | {":p2": "dict2", ":p1": "dict1", ":p3": "dict3"})

lk(name = "two_selects", m = select({"//conditions:default": {":p2": "a2", ":p1": "a1"}}) | select({":opt": {":p3": "b3"}, "//conditions:default": {":p1": "b1"}}))
