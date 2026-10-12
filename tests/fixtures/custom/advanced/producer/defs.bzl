load("//advanced/provider_defs:defs.bzl", ProducerPayload = "PayloadInfo")

def _produce_impl(ctx):
    out = ctx.actions.declare_file("provider-only.txt")
    ctx.actions.write(out, "provider payload\n")
    return [
        DefaultInfo(files = depset([])),
        ProducerPayload(
            file = out,
            message = "provider message",
            words = ["one", "two"],
            metadata = struct(tag = "nested", numbers = [3, 5]),
        ),
    ]

produce = rule(implementation = _produce_impl)
