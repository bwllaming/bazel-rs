"""Validation output group fixtures (N56, docs/VALIDATION_ACTIONS.md).

`val` is the shape the real i15 boundary exercised: a rule whose
implementation returns its validation outputs in the `_validation` output
group directly (rules_go's go_library does exactly this,
go/private/rules/library.bzl:69-75). `middle` models a consuming rule with
target/exec/private/dict dependency attributes so the framework's
transitive collection (RuleConfiguredTargetBuilder.
collectTransitiveValidationOutputGroups) is observable per attribute kind.
"""

def _val(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".validated")
    ctx.actions.write(out, "validated:" + ctx.label.name)
    return [
        DefaultInfo(files = depset([out])),
        OutputGroupInfo(_validation = depset([out])),
    ]

val = rule(implementation = _val)

# Like `val`, but with a rule-level named output: `//:vout.out` is an
# output-file target whose OutputGroupInfo is the generating rule's
# `_validation` group (OutputFileConfiguredTarget.java:109-118).
def _val_out(ctx):
    ctx.actions.write(ctx.outputs.out, "vout")
    validated = ctx.actions.declare_file(ctx.label.name + ".validated")
    ctx.actions.write(validated, "validated:" + ctx.label.name)
    return [
        DefaultInfo(files = depset([ctx.outputs.out])),
        OutputGroupInfo(_validation = depset([validated])),
    ]

val_out = rule(
    implementation = _val_out,
    outputs = {"out": "%{name}.out"},
)

def _middle(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "middle")
    validation = []
    if ctx.attr.own:
        own = ctx.actions.declare_file(ctx.label.name + ".mine")
        ctx.actions.write(own, "mine")
        validation = [own]
    return [
        DefaultInfo(files = depset([out])),
        OutputGroupInfo(_validation = depset(validation)),
    ]

middle = rule(
    implementation = _middle,
    attrs = {
        "direct": attr.label(),
        "tool": attr.label(cfg = "exec"),
        "own": attr.bool(),
    },
)

# A private `_`-prefixed attribute is NOT implicit (only `$` names are,
# Attribute.java:2273-2275): its validation outputs are collected like any
# explicit dependency's.
private_collector = rule(
    implementation = _middle,
    attrs = {
        "_hidden": attr.label(default = Label("//:leaf")),
        "own": attr.bool(),
    },
)

# label_list_dict is a DEPENDENCY-class attribute type: collected.
dict_collector = rule(
    implementation = _middle,
    attrs = {
        "mapping": attr.label_list_dict(),
        "own": attr.bool(),
    },
)

# Consumes the generated file //:leaf.out: the output-file target's
# OutputGroupInfo is its generating rule's `_validation` group
# (OutputFileConfiguredTarget.java:109-118), so the framework collects
# through it.
def _via_file(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "via-file")
    return [DefaultInfo(files = depset([out]))]

via_file = rule(
    implementation = _via_file,
    attrs = {"dep": attr.label(allow_single_file = True)},
)

def _g(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "default")
    extra = ctx.actions.declare_file(ctx.label.name + ".extra")
    ctx.actions.write(extra, "extra")
    return [
        DefaultInfo(files = depset([out])),
        OutputGroupInfo(extra = depset([extra])),
    ]

g = rule(implementation = _g)

def _plain(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "plain")
    return [DefaultInfo(files = depset([out]))]

plain = rule(implementation = _plain)

def _val_fail(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".validated")
    ctx.actions.run_shell(
        outputs = [out],
        command = "printf broken > $0; exit 1",
        arguments = [out.path],
    )
    return [
        DefaultInfo(files = depset([])),
        OutputGroupInfo(_validation = depset([out])),
    ]

val_fail = rule(implementation = _val_fail)

# Returning the private `_validation_transitive` group from a user rule is
# rejected outside BuiltinRestriction's internal allowlist
# (RuleConfiguredTargetBuilder.java:366-377).
def _transitive(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".validated")
    ctx.actions.write(out, "x")
    return [OutputGroupInfo(_validation_transitive = depset([out]))]

transitive = rule(implementation = _transitive)

# A skip_validations attribute: genuine Bazel skips its validation outputs;
# the native profile does not analyze such rules (declared feature gap), so
# targets of this rule are genuine-only phases.
def _skipper(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.write(out, "skipped")
    return [DefaultInfo(files = depset([out]))]

skipper = rule(
    implementation = _skipper,
    attrs = {"skipped": attr.label(skip_validations = True)},
)
