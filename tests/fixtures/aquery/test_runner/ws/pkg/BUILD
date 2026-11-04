load(":defs.bzl", "my_bin", "my_test", "req_test")

my_test(name = "t", data = ["d.txt"], args = ["a b", "$(location d.txt)"])

my_test(
    name = "s",
    env2 = {"Z": "1", "A": "b"},
    inherit = ["HOME", "USER"],
    shard_count = 2,
    size = "large",
    tags = ["requires-network", "foo", "no-cache", "block-network", "foo"],
)

my_test(name = "f", flaky = True, tags = ["exclusive"], timeout = "short")

my_test(name = "l", local = True, size = "small")

req_test(name = "r", tags = ["no-remote", "cpu:2"])

my_bin(name = "wrap")

genrule(name = "g", outs = ["g.txt"], cmd = "touch $@")
