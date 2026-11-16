load(":defs.bzl", "slow_test")

# Three slow genrules in parallel, a fourth after the first.
[genrule(
    name = "slow%d" % i,
    outs = ["slow%d.txt" % i],
    cmd = "sleep %d; echo %d > $@" % (2 + i, i),
) for i in range(3)]

genrule(
    name = "chain",
    srcs = [":slow0"],
    outs = ["chain.txt"],
    cmd = "sleep 2; cat $< > $@",
)

# Twenty packages and a repository rule that takes two seconds.
filegroup(
    name = "all",
    srcs = [
        ":chain",
        ":slow1",
        ":slow2",
        "@slow//:r",
    ] + ["//p%d:g" % i for i in range(20)],
)

# One action past the inactivity watchdog's first reminder.
genrule(
    name = "long",
    outs = ["long.txt"],
    cmd = "sleep 13; echo > $@",
)

slow_test(
    name = "t1",
    sleep = "2",
)

slow_test(
    name = "t2",
    sleep = "4",
)

slow_test(
    name = "t3",
    code = "1",
    sleep = "3",
)
